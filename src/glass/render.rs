//! Offscreen liquid-glass rendering over a captured backdrop.
//!
//! One wgpu device per bar, one offscreen `Rgba8UnormSrgb` target, one GPU→CPU
//! readback into a `gdk::MemoryTexture`. The zero-copy dmabuf path would avoid
//! the readback stall but needs `wgpu::hal::vulkan` unsafe interop and a dmabuf
//! importing GSK renderer, so it stays out of scope for this feature.

use std::time::Instant;

use liquid_rust::{Color, Frame, Glass, Material, Rect, Renderer, Scene, Shape};

use super::config::{GlassAdapter, GlassConfig, GlassPreset, Tint};

/// Every texture this module creates.
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

/// A module pill to render on top of the bar capsule, in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pill {
    /// Left edge.
    pub x: f32,
    /// Top edge.
    pub y: f32,
    /// Width.
    pub width: f32,
    /// Height.
    pub height: f32,
}

/// One rendered frame plus the timings of the three stages.
#[derive(Debug, Clone)]
pub struct Rendered {
    /// Tightly packed RGBA8 pixels of the glass output.
    pub rgba: Vec<u8>,
    /// Milliseconds spent in the glass shader.
    pub draw_ms: f64,
    /// Milliseconds spent uploading the backdrop to the GPU.
    pub upload_ms: f64,
    /// Milliseconds spent in the GPU→CPU readback (a pipeline stall).
    pub readback_ms: f64,
}

/// Which adapter was picked and what the driver reports.
#[derive(Debug, Clone)]
pub struct AdapterSummary {
    /// Human readable adapter name.
    pub name: String,
    /// Backend (Vulkan, GL, ...).
    pub backend: String,
    /// Integrated, discrete, virtual, ...
    pub device_type: String,
    /// Driver name and version, if reported.
    pub driver: String,
}

impl std::fmt::Display for AdapterSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} [{} {} driver={}]",
            self.name, self.backend, self.device_type, self.driver
        )
    }
}

/// Picks an adapter name from the enumerated list for a preference.
///
/// `preference` is the index resolved by [`select_adapter`]; this helper exists
/// so the ordering rules are testable without a GPU.
pub fn adapter_order(types: &[wgpu::DeviceType], preference: GlassAdapter) -> Vec<usize> {
    let integrated = types
        .iter()
        .enumerate()
        .filter(|(_, ty)| **ty == wgpu::DeviceType::IntegratedGpu)
        .map(|(i, _)| i);
    let discrete = types
        .iter()
        .enumerate()
        .filter(|(_, ty)| **ty == wgpu::DeviceType::DiscreteGpu)
        .map(|(i, _)| i);
    let rest = types
        .iter()
        .enumerate()
        .filter(|(_, ty)| {
            !matches!(
                ty,
                wgpu::DeviceType::IntegratedGpu | wgpu::DeviceType::DiscreteGpu
            )
        })
        .map(|(i, _)| i);

    let mut order: Vec<usize> = match preference {
        GlassAdapter::Integrated | GlassAdapter::Auto => {
            integrated.chain(discrete).chain(rest).collect()
        }
        GlassAdapter::Discrete => discrete.chain(integrated).chain(rest).collect(),
    };

    order.dedup();
    order
}

/// A wgpu device plus the liquid-rust renderer state for one surface size.
pub struct GlassRenderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    renderer: Renderer,
    scene: Scene,
    info: AdapterSummary,

    size: (u32, u32),
    content: wgpu::Texture,
    target: wgpu::Texture,
    readback: wgpu::Buffer,

    pills: Vec<Pill>,
    radius: f32,
    preset: GlassPreset,
    tint: Option<Color>,
}

impl GlassRenderer {
    /// Creates a renderer on the best adapter for `config.adapter`.
    pub fn new(config: &GlassConfig) -> Result<Self, String> {
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());

        let adapters = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::all()));
        if adapters.is_empty() {
            return Err("no wgpu adapters found".to_owned());
        }

        let types: Vec<wgpu::DeviceType> =
            adapters.iter().map(|a| a.get_info().device_type).collect();
        let order = adapter_order(&types, config.adapter);
        let index = *order
            .first()
            .ok_or_else(|| "no usable wgpu adapter".to_owned())?;

        let adapter = adapters
            .into_iter()
            .nth(index)
            .ok_or_else(|| "adapter vanished".to_owned())?;
        let raw = adapter.get_info();
        let info = AdapterSummary {
            name: raw.name.clone(),
            backend: format!("{:?}", raw.backend),
            device_type: format!("{:?}", raw.device_type),
            driver: format!("{} {}", raw.driver, raw.driver_info),
        };

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("ironbar-glass"),
            ..Default::default()
        }))
        .map_err(|err| format!("could not create wgpu device on '{}': {err}", raw.name))?;

        let (_, _, content, target, readback) = Self::allocate(&device, (1, 1));

        let mut scene = Scene::new();
        scene.physics = false;
        scene.glass_on_glass = true;
        scene.appearance.dark = true;

        Ok(Self {
            renderer: Renderer::new(&device, FORMAT),
            device,
            queue,
            scene,
            info,
            size: (1, 1),
            content,
            target,
            readback,
            pills: Vec::new(),
            radius: 0.0,
            preset: config.preset,
            tint: config.tint.map(to_color),
        })
    }

    /// The adapter this renderer runs on.
    pub fn adapter(&self) -> &AdapterSummary {
        &self.info
    }

    fn allocate(
        device: &wgpu::Device,
        (width, height): (u32, u32),
    ) -> (u32, u32, wgpu::Texture, wgpu::Texture, wgpu::Buffer) {
        let extent = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let texture = |usage| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some("ironbar-glass"),
                size: extent,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: FORMAT,
                usage,
                view_formats: &[],
            })
        };

        let content = texture(wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST);
        let target =
            texture(wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC);

        let row = (4 * width).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ironbar-glass-readback"),
            size: u64::from(row) * u64::from(height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        (width, height, content, target, readback)
    }

    /// Sets the pill rectangles and bar radius for the next render, in physical px.
    pub fn set_layout(&mut self, radius: f32, pills: Vec<Pill>) {
        self.radius = radius;
        self.pills = pills;
    }

    /// Rebuilds the liquid-rust scene for the current size and layout.
    fn rebuild_scene(&mut self, scale: f32) {
        let (width, height) = (self.size.0 as f32 / scale, self.size.1 as f32 / scale);
        let radius = (self.radius / scale).clamp(0.0, height / 2.0);
        let material = self.material();

        let mut scene = Scene::new();
        scene.physics = false;
        scene.glass_on_glass = true;
        scene.appearance.dark = true;

        let container = scene.add_container(12.0, 0);
        scene.add(
            Glass::new(Rect::new(0.0, 0.0, width, height))
                .shape(Shape::Rounded(radius))
                .material(material)
                .container(container),
        );

        for pill in &self.pills {
            scene.add(
                Glass::new(Rect::new(
                    pill.x / scale,
                    pill.y / scale,
                    pill.width / scale,
                    pill.height / scale,
                ))
                .shape(Shape::Rounded(pill.height / scale / 2.0))
                .material(Material::clear())
                .z(1)
                .container(container),
            );
        }

        self.scene = scene;
        // Springs start settled so the first frame already has a fully packed glass.
        while self.scene.update(1.0 / 60.0) {}
    }

    fn material(&self) -> Material {
        match self.preset {
            GlassPreset::Regular => Material::regular(),
            GlassPreset::Clear => Material::clear(),
            GlassPreset::Tinted => Material::tinted(self.tint.unwrap_or(Color::BLUE)),
        }
    }

    /// Renders `backdrop` (tightly packed RGBA8, physical pixels) into the glass target.
    pub fn render(
        &mut self,
        backdrop: &[u8],
        size: (u32, u32),
        scale: f32,
    ) -> Result<Rendered, String> {
        if size.0 == 0 || size.1 == 0 {
            return Err("empty glass surface".to_owned());
        }

        if self.size != size {
            let (_, _, content, target, readback) = Self::allocate(&self.device, size);
            self.size = size;
            self.content = content;
            self.target = target;
            self.readback = readback;
        }

        let expected = size.0 as usize * size.1 as usize * 4;
        if backdrop.len() < expected {
            return Err(format!(
                "backdrop is {} bytes, need {expected} for {}x{}",
                backdrop.len(),
                size.0,
                size.1
            ));
        }

        self.rebuild_scene(scale);

        let extent = wgpu::Extent3d {
            width: size.0,
            height: size.1,
            depth_or_array_layers: 1,
        };
        let layout = wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(4 * size.0),
            rows_per_image: Some(size.1),
        };

        let upload_started = Instant::now();
        self.queue.write_texture(
            self.content.as_image_copy(),
            &backdrop[..expected],
            layout,
            extent,
        );
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|err| format!("upload poll: {err:?}"))?;
        let upload_ms = upload_started.elapsed().as_secs_f64() * 1000.0;

        let content = self.content.create_view(&Default::default());
        let target = self.target.create_view(&Default::default());

        let frame = Frame {
            content: &content,
            target: &target,
            size,
            scale,
            content_changed: true,
        };

        let draw_started = Instant::now();
        let mut encoder = self.device.create_command_encoder(&Default::default());
        self.renderer
            .render(&self.device, &self.queue, &mut encoder, &frame, &self.scene);
        self.queue.submit([encoder.finish()]);
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|err| format!("draw poll: {err:?}"))?;
        let draw_ms = draw_started.elapsed().as_secs_f64() * 1000.0;

        let row = (4 * size.0).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
        let readback_started = Instant::now();
        let mut encoder = self.device.create_command_encoder(&Default::default());
        encoder.copy_texture_to_buffer(
            self.target.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &self.readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row),
                    rows_per_image: Some(size.1),
                },
            },
            extent,
        );
        self.queue.submit([encoder.finish()]);

        let slice = self.readback.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|err| format!("readback poll: {err:?}"))?;

        let mut rgba = Vec::with_capacity(expected);
        {
            let data = slice
                .get_mapped_range()
                .map_err(|err| format!("map failed: {err}"))?;
            if row == 4 * size.0 {
                rgba.extend_from_slice(&data);
            } else {
                for y in 0..size.1 {
                    let start = (y * row) as usize;
                    rgba.extend_from_slice(&data[start..start + 4 * size.0 as usize]);
                }
            }
        }
        self.readback.unmap();
        let readback_ms = readback_started.elapsed().as_secs_f64() * 1000.0;

        Ok(Rendered {
            rgba,
            draw_ms,
            upload_ms,
            readback_ms,
        })
    }
}

fn to_color(tint: Tint) -> Color {
    Color {
        r: f32::from(tint.r) / 255.0,
        g: f32::from(tint.g) / 255.0,
        b: f32::from(tint.b) / 255.0,
        a: f32::from(tint.a) / 255.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_prefers_integrated_then_discrete() {
        use wgpu::DeviceType::{Cpu, DiscreteGpu, IntegratedGpu};

        let types = [DiscreteGpu, IntegratedGpu];
        assert_eq!(adapter_order(&types, GlassAdapter::Auto), vec![1, 0]);

        let types = [DiscreteGpu];
        assert_eq!(adapter_order(&types, GlassAdapter::Auto), vec![0]);

        let types = [Cpu, DiscreteGpu, IntegratedGpu];
        assert_eq!(adapter_order(&types, GlassAdapter::Auto), vec![2, 1, 0]);
    }

    #[test]
    fn discrete_preference_flips_the_order() {
        use wgpu::DeviceType::{DiscreteGpu, IntegratedGpu};

        let types = [IntegratedGpu, DiscreteGpu];
        assert_eq!(adapter_order(&types, GlassAdapter::Discrete), vec![1, 0]);
        assert_eq!(adapter_order(&types, GlassAdapter::Integrated), vec![0, 1]);
    }

    #[test]
    fn every_adapter_is_considered() {
        use wgpu::DeviceType::{Cpu, DiscreteGpu, IntegratedGpu, VirtualGpu};

        let types = [VirtualGpu, IntegratedGpu, Cpu, DiscreteGpu];
        let order = adapter_order(&types, GlassAdapter::Discrete);
        assert_eq!(order.len(), 4, "no adapter may be dropped: {order:?}");
        assert_eq!(order, vec![3, 1, 0, 2]);
    }

    #[test]
    fn tint_converts_to_linear_scale() {
        let color = to_color(Tint::from_rgba(0x33_66_99, 0x80));
        assert!((color.r - 0.2).abs() < 1e-6);
        assert!((color.g - 0.4).abs() < 1e-6);
        assert!((color.b - 0.6).abs() < 1e-6);
        assert!((color.a - 128.0 / 255.0).abs() < 1e-6);
    }
}
