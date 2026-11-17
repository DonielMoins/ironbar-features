//! Offscreen liquid-glass rendering over a captured backdrop.
//!
//! One wgpu device per bar, one offscreen `Rgba8UnormSrgb` target, one GPU→CPU
//! readback into a `gdk::MemoryTexture`. The zero-copy dmabuf path would avoid
//! the readback stall but needs `wgpu::hal::vulkan` unsafe interop and a dmabuf
//! importing GSK renderer, so it stays out of scope for this feature.

use std::time::Instant;

use liquid_rust::{Color, Frame, Glass, Material, Rect, Renderer, Scene, Shadow, Shape};

use crate::{Adapter as GlassAdapter, GlassConfig, Preset, Rgba};

/// Every texture this module creates.
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

/// A pill (e.g. one bar module) rendered on top of the main capsule, in
/// physical pixels relative to the window.
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
    /// Tightly packed RGBA8 (straight alpha) of the window rectangle.
    pub rgba: Vec<u8>,
    /// Width of `rgba` in physical pixels.
    pub width: u32,
    /// Height of `rgba` in physical pixels.
    pub height: u32,
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
    /// Window rectangle inside the (padded) backdrop, physical pixels.
    inner: (u32, u32, u32, u32),
    preset: Preset,
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
            label: Some("gtk-liquid-glass"),
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
            inner: (0, 0, 1, 1),
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
                label: Some("gtk-liquid-glass"),
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
            label: Some("gtk-liquid-glass-readback"),
            size: u64::from(row) * u64::from(height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        (width, height, content, target, readback)
    }

    /// Sets the window rectangle inside the backdrop, its corner radius and the
    /// pill rectangles for the next render. All values are physical pixels;
    /// `inner` is `(x, y, width, height)` of the window in the backdrop and
    /// pills are relative to the window.
    pub fn set_layout(&mut self, inner: (u32, u32, u32, u32), radius: f32, pills: Vec<Pill>) {
        self.inner = inner;
        self.radius = radius;
        self.pills = pills;
    }

    /// Rebuilds the liquid-rust scene for the current size and layout.
    fn rebuild_scene(&mut self, scale: f32) {
        let (ix, iy, iw, ih) = self.inner;
        let (x, y) = (ix as f32 / scale, iy as f32 / scale);
        let (width, height) = (iw as f32 / scale, ih as f32 / scale);
        let radius = (self.radius / scale).clamp(0.0, width.min(height) / 2.0);

        let mut scene = Scene::new();
        scene.physics = false;
        // Glass may not sample glass: with nesting on, each pill refracts the
        // rendered capsule (and its neighbours through the container merge),
        // which draws lens wedges between the pills.
        scene.glass_on_glass = false;
        scene.appearance.dark = true;

        scene.add(
            Glass::new(Rect::new(x, y, width, height))
                .shape(Shape::Rounded(radius))
                .material(surface_material(self.preset, self.tint, height)),
        );

        for pill in &self.pills {
            let h = pill.height / scale;
            scene.add(
                Glass::new(Rect::new(
                    x + pill.x / scale,
                    y + pill.y / scale,
                    pill.width / scale,
                    h,
                ))
                .shape(Shape::Rounded(h / 2.0))
                .material(pill_material(h))
                .z(1),
            );
        }

        self.scene = scene;
        // Springs start settled so the first frame already has a fully packed glass.
        while self.scene.update(1.0 / 60.0) {}
    }

    /// Renders `backdrop` (tightly packed RGBA8 of `size` physical pixels, the
    /// window plus lens padding) and returns the window rectangle only, with
    /// everything outside the rounded corners made transparent.
    pub fn render(
        &mut self,
        backdrop: &[u8],
        size: (u32, u32),
        scale: f32,
    ) -> Result<Rendered, String> {
        if size.0 == 0 || size.1 == 0 {
            return Err("empty glass surface".to_owned());
        }

        let (ix, iy, iw, ih) = self.inner;
        if iw == 0 || ih == 0 || ix + iw > size.0 || iy + ih > size.1 {
            return Err(format!(
                "window rect {:?} does not fit the {}x{} backdrop",
                self.inner, size.0, size.1
            ));
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

        // Read back only the window rectangle: the padding exists for the lens,
        // never for display.
        let row = (4 * iw).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
        let readback_started = Instant::now();
        let mut encoder = self.device.create_command_encoder(&Default::default());
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.target,
                mip_level: 0,
                origin: wgpu::Origin3d { x: ix, y: iy, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &self.readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row),
                    rows_per_image: Some(ih),
                },
            },
            wgpu::Extent3d {
                width: iw,
                height: ih,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit([encoder.finish()]);

        let slice = self.readback.slice(..u64::from(row) * u64::from(ih));
        slice.map_async(wgpu::MapMode::Read, |_| {});
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|err| format!("readback poll: {err:?}"))?;

        let mut rgba = Vec::with_capacity(iw as usize * ih as usize * 4);
        {
            let data = slice
                .get_mapped_range()
                .map_err(|err| format!("map failed: {err}"))?;
            for y in 0..ih {
                let start = (y * row) as usize;
                rgba.extend_from_slice(&data[start..start + 4 * iw as usize]);
            }
        }
        self.readback.unmap();
        let readback_ms = readback_started.elapsed().as_secs_f64() * 1000.0;

        let radius = self.radius.clamp(0.0, iw.min(ih) as f32 / 2.0);
        mask_rounded(&mut rgba, iw, ih, radius);

        Ok(Rendered {
            rgba,
            width: iw,
            height: ih,
            draw_ms,
            upload_ms,
            readback_ms,
        })
    }
}

/// The main capsule material, tuned for a thin bar.
///
/// liquid-rust's presets are drawn for large iOS surfaces: a 30 pt bevel with
/// 0.7 refraction. On a 38 pt capsule that bevel covers the whole rounded end,
/// so the lens samples far outside the captured strip and the clamped edge
/// texels show up as hard black/white wedges at the capsule ends. The bevel is
/// kept to a third of the height, refraction and dispersion are lowered and the
/// drop shadow (clipped by the window anyway) is dropped.
pub fn surface_material(preset: Preset, tint: Option<Color>, height: f32) -> Material {
    let base = match preset {
        Preset::Regular => Material::regular(),
        Preset::Clear => Material::clear(),
    };

    Material {
        refraction: 0.35,
        depth: (height / 3.0).clamp(4.0, 14.0),
        dispersion: 0.04,
        specular: 0.18,
        rim: 0.08,
        rim_shade: 0.0,
        edge: 0.08,
        shadow: Shadow {
            offset_y: 0.0,
            blur: 0.0,
            opacity: 0.0,
        },
        tint,
        // Always the dark tone: the adaptive flip turns a thin bar light grey
        // over a dark desktop, which drowns white labels.
        adaptive: false,
        light_tone: base.dark_tone,
        ..base
    }
}

/// Module pill material: a lighter, flatter glass so each pill reads as its own
/// surface on top of the ink capsule without competing with its label.
pub fn pill_material(height: f32) -> Material {
    Material {
        refraction: 0.2,
        depth: (height / 4.0).clamp(3.0, 8.0),
        dispersion: 0.0,
        frost: 8.0,
        specular: 0.12,
        rim: 0.06,
        rim_shade: 0.0,
        edge: 0.05,
        shadow: Shadow {
            offset_y: 0.0,
            blur: 0.0,
            opacity: 0.0,
        },
        // Ink lifted by ~15 %: same family as the capsule, one step lighter, so
        // each module reads as its own surface and white labels keep contrast.
        tint: Some(Color {
            r: 0.24,
            g: 0.25,
            b: 0.245,
            a: 0.55,
        }),
        adaptive: false,
        light_tone: Material::regular().dark_tone,
        ..Material::regular()
    }
}

/// Clears everything outside a rounded rectangle of `radius` (physical px),
/// with one pixel of anti-aliasing. Straight alpha.
pub fn mask_rounded(rgba: &mut [u8], width: u32, height: u32, radius: f32) {
    if radius <= 0.0 {
        return;
    }

    let (w, h) = (width as f32, height as f32);
    let corner = radius.ceil() as u32 + 1;

    for y in 0..height {
        let near_y = y < corner || y + corner >= height;
        if !near_y {
            continue;
        }
        for x in (0..width).filter(|&x| x < corner || x + corner >= width) {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;
            let cx = px.clamp(radius, w - radius);
            let cy = py.clamp(radius, h - radius);
            let dist = ((px - cx).powi(2) + (py - cy).powi(2)).sqrt();
            let coverage = (radius - dist + 0.5).clamp(0.0, 1.0);
            let alpha = &mut rgba[((y * width + x) * 4 + 3) as usize];
            *alpha = (f32::from(*alpha) * coverage).round() as u8;
        }
    }
}

fn to_color(tint: Rgba) -> Color {
    Color {
        r: tint.r,
        g: tint.g,
        b: tint.b,
        a: tint.a,
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
        let color = to_color(Rgba::from_u8(0x33, 0x66, 0x99, 0.5));
        assert!((color.r - 0.2).abs() < 1e-6);
        assert!((color.g - 0.4).abs() < 1e-6);
        assert!((color.b - 0.6).abs() < 1e-6);
        assert!((color.a - 0.5).abs() < 1e-6);
    }

    #[test]
    fn mask_clears_corners_and_keeps_the_body() {
        let (w, h) = (80u32, 38u32);
        let mut px = vec![255u8; (w * h * 4) as usize];
        mask_rounded(&mut px, w, h, 19.0);
        let a = |x: u32, y: u32| px[((y * w + x) * 4 + 3) as usize];
        assert_eq!(a(0, 0), 0, "corner pixel is outside the capsule");
        assert_eq!(a(w - 1, h - 1), 0);
        assert_eq!(a(w / 2, 0), 255, "top edge centre is inside");
        assert!(a(0, h / 2) >= 250, "capsule end centre is inside");
        assert_eq!(a(40, 19), 255);
    }

    #[test]
    fn bar_material_keeps_the_lens_inside_the_capsule() {
        let m = surface_material(Preset::Regular, Some(to_color(Rgba::INK)), 38.0);
        assert!(m.depth <= 38.0 / 2.0, "bevel must not span a capsule end");
        assert!(m.refraction < Material::regular().refraction);
        assert_eq!(m.shadow.opacity, 0.0);
        assert!(m.tint.is_some());
    }
}
