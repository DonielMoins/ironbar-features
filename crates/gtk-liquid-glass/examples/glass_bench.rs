//! Benchmark for the `liquid-glass` renderer at real bar sizes, on every GPU
//! adapter the machine exposes.
//!
//! This deliberately does not touch the compositor: a synthetic backdrop is
//! uploaded through the same path a real capture takes, so the numbers isolate
//! glass shader + upload + readback cost and can be compared across adapters
//! and machines.
//!
//! ```sh
//! cargo +stable run --release --features liquid-glass --example glass_bench
//! ```

use std::time::{Duration, Instant};

use liquid_rust::{Color, Frame, Glass, Material, Rect, Renderer, Scene, Shape};

const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;
const FRAMES: u32 = 60;

/// A bar strip at 2x on a 1920pt-wide 4K output.
const BAR: (u32, u32) = (3820, 76);
/// A control-centre sized panel at 2x.
const PANEL: (u32, u32) = (840, 1240);

/// Synthetic desktop: a vertical gradient plus a few bright bands, so the glass
/// has real structure to refract (the shader cost does not depend on content,
/// but a flat fill makes a wrong render look correct).
fn backdrop(width: u32, height: u32) -> Vec<u8> {
    let mut px = Vec::with_capacity((width * height * 4) as usize);

    for y in 0..height {
        for x in 0..width {
            let fx = x as f32 / width as f32;
            let fy = y as f32 / height as f32;
            let band = ((fx * 9.0).sin() * 0.5 + 0.5) * 60.0;
            px.push((30.0 + band + fy * 40.0) as u8);
            px.push((40.0 + fx * 60.0) as u8);
            px.push((70.0 + (1.0 - fy) * 80.0) as u8);
            px.push(255);
        }
    }

    px
}

fn bar_scene() -> Scene {
    let mut scene = Scene::new();
    scene.physics = false;
    scene.glass_on_glass = true;
    scene.appearance.dark = true;

    let container = scene.add_container(12.0, 0);
    scene.add(
        Glass::new(Rect::new(0.0, 0.0, 1910.0, 38.0))
            .shape(Shape::Rounded(19.0))
            .material(Material::regular())
            .container(container),
    );

    // Workspace pill, clock pill, four status pills: the real bar's shape.
    scene.add(
        Glass::new(Rect::new(12.0, 6.0, 156.0, 26.0))
            .shape(Shape::Rounded(13.0))
            .material(Material::clear())
            .z(1)
            .container(container),
    );
    scene.add(
        Glass::new(Rect::new(880.0, 6.0, 150.0, 26.0))
            .shape(Shape::Rounded(13.0))
            .material(Material::clear())
            .z(1)
            .container(container),
    );
    for i in 0..4 {
        scene.add(
            Glass::new(Rect::new(1700.0 + 44.0 * i as f32, 6.0, 34.0, 26.0))
                .shape(Shape::Rounded(13.0))
                .material(Material::clear())
                .z(1)
                .container(container),
        );
    }

    scene
}

fn panel_scene() -> Scene {
    let mut scene = Scene::new();
    scene.physics = false;
    scene.glass_on_glass = true;
    scene.appearance.dark = true;

    let container = scene.add_container(10.0, 0);
    scene.add(
        Glass::new(Rect::new(0.0, 0.0, 420.0, 620.0))
            .shape(Shape::Rounded(19.0))
            .material(Material::regular())
            .container(container),
    );

    for (i, y) in [16.0f32, 120.0, 224.0].iter().enumerate() {
        for (j, x) in [16.0f32, 214.0].iter().enumerate() {
            if i == 2 && j == 1 {
                continue;
            }
            scene.add(
                Glass::new(Rect::new(*x, *y, 190.0, 96.0))
                    .shape(Shape::Rounded(16.0))
                    .material(Material::clear())
                    .z(1)
                    .container(container),
            );
        }
    }

    scene.add(
        Glass::new(Rect::new(250.0, 148.0, 140.0, 40.0))
            .shape(Shape::Rounded(20.0))
            .material(Material::tinted(Color::YELLOW))
            .z(1)
            .container(container),
    );

    scene
}

struct Case {
    label: &'static str,
    size: (u32, u32),
    scale: f32,
    scene: Scene,
}

/// Renders one case `FRAMES` times and reports glass / upload / readback costs.
fn run_case(device: &wgpu::Device, queue: &wgpu::Queue, case: &mut Case) -> (f64, f64, f64) {
    let (width, height) = case.size;
    let extent = wgpu::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    };

    let texture = |usage| {
        device.create_texture(&wgpu::TextureDescriptor {
            label: None,
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
    let target = texture(wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC);

    let pixels = backdrop(width, height);
    let layout = wgpu::TexelCopyBufferLayout {
        offset: 0,
        bytes_per_row: Some(4 * width),
        rows_per_image: Some(height),
    };

    // First-touch costs (staging buffers, the readback buffer's first map) are
    // excluded from the timed loops; they happen once per surface size.
    queue.write_texture(content.as_image_copy(), &pixels, layout, extent);

    let upload_started = Instant::now();
    for _ in 0..FRAMES {
        queue.write_texture(content.as_image_copy(), &pixels, layout, extent);
    }
    device.poll(wgpu::PollType::wait_indefinitely()).ok();
    let upload_ms = upload_started.elapsed().as_secs_f64() * 1000.0 / f64::from(FRAMES);

    let content_view = content.create_view(&Default::default());
    let target_view = target.create_view(&Default::default());
    let mut renderer = Renderer::new(device, FORMAT);

    // Springs must be stepped once before the first render or nothing is packed.
    while case.scene.update(1.0 / 60.0) {}

    let frame = Frame {
        content: &content_view,
        target: &target_view,
        size: (width, height),
        scale: case.scale,
        content_changed: true,
    };

    for _ in 0..3 {
        let mut encoder = device.create_command_encoder(&Default::default());
        renderer.render(device, queue, &mut encoder, &frame, &case.scene);
        queue.submit([encoder.finish()]);
    }
    device.poll(wgpu::PollType::wait_indefinitely()).ok();

    let row = (4 * width).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: u64::from(row) * u64::from(height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let draw_started = Instant::now();
    for _ in 0..FRAMES {
        let mut encoder = device.create_command_encoder(&Default::default());
        renderer.render(device, queue, &mut encoder, &frame, &case.scene);
        queue.submit([encoder.finish()]);
    }
    device.poll(wgpu::PollType::wait_indefinitely()).ok();
    let draw_ms = draw_started.elapsed().as_secs_f64() * 1000.0 / f64::from(FRAMES);

    // Warm the readback path once so the timed pass measures steady state.
    let mut encoder = device.create_command_encoder(&Default::default());
    encoder.copy_texture_to_buffer(
        target.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(row),
                rows_per_image: Some(height),
            },
        },
        extent,
    );
    queue.submit([encoder.finish()]);
    readback.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    device.poll(wgpu::PollType::wait_indefinitely()).ok();
    let _ = readback.slice(..).get_mapped_range().map(|data| data.len());
    readback.unmap();

    let readback_started = Instant::now();
    let mut encoder = device.create_command_encoder(&Default::default());
    encoder.copy_texture_to_buffer(
        target.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(row),
                rows_per_image: Some(height),
            },
        },
        extent,
    );
    queue.submit([encoder.finish()]);
    readback.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    device.poll(wgpu::PollType::wait_indefinitely()).ok();
    let _ = readback.slice(..).get_mapped_range().map(|data| data.len());
    readback.unmap();
    let readback_ms = readback_started.elapsed().as_secs_f64() * 1000.0;

    println!(
        "  {:<22} {:>5}x{:<5} glass {:>7.3} ms/f   upload {:>6.2} ms   readback {:>6.2} ms   ({:.1} MiB)",
        case.label,
        width,
        height,
        draw_ms,
        upload_ms,
        readback_ms,
        f64::from(4 * width * height) / 1_048_576.0
    );

    (draw_ms, upload_ms, readback_ms)
}

fn runtime_status() -> String {
    std::fs::read_to_string("/sys/bus/pci/devices/0000:01:00.0/power/runtime_status")
        .map(|s| s.trim().to_owned())
        .unwrap_or_else(|_| "n/a".to_owned())
}

fn runtime_suspended_ms() -> Option<u64> {
    std::fs::read_to_string("/sys/bus/pci/devices/0000:01:00.0/power/runtime_suspended_time")
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn main() {
    let instance =
        wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());

    let adapters = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::all()));
    println!("{} wgpu adapter(s) found", adapters.len());

    let dgpu_before = runtime_status();
    println!("dGPU 0000:01:00.0 runtime_status before: {dgpu_before}\n");

    let mut failures = 0;

    for (index, adapter) in adapters.into_iter().enumerate() {
        let info = adapter.get_info();
        println!(
            "adapter #{index}: {} | {:?} | {:?} | driver '{}' '{}'",
            info.name, info.backend, info.device_type, info.driver, info.driver_info
        );

        let suspended_before = runtime_suspended_ms();

        let Ok((device, queue)) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("glass_bench"),
                ..Default::default()
            }))
        else {
            println!("  device creation failed, skipping\n");
            failures += 1;
            continue;
        };

        let mut bar = Case {
            label: "bar",
            size: BAR,
            scale: 2.0,
            scene: bar_scene(),
        };
        let mut panel = Case {
            label: "control-centre",
            size: PANEL,
            scale: 2.0,
            scene: panel_scene(),
        };

        run_case(&device, &queue, &mut bar);
        run_case(&device, &queue, &mut panel);

        // Let the driver settle, then look at the power state it chose.
        std::thread::sleep(Duration::from_millis(500));
        let suspended_after = runtime_suspended_ms();
        println!(
            "  dGPU runtime_status after: {} (suspended {:.1} ms -> {:.1} ms during the run)",
            runtime_status(),
            suspended_before.unwrap_or(0) as f64 / 1000.0,
            suspended_after.unwrap_or(0) as f64 / 1000.0,
        );

        drop(device);
        println!();
    }

    println!("dGPU runtime_status at exit: {}", runtime_status());

    if failures > 0 {
        eprintln!("{failures} adapter(s) could not be initialised");
    }
}
