//! Screen-region capture for the glass backdrop.
//!
//! Uses `zwlr_screencopy_manager_v1` with a `wl_shm` buffer: the compositor
//! copies the region under the bar into memory we own, on a dedicated wayland
//! connection (and therefore a dedicated thread — a capture is a blocking round
//! trip of roughly one compositor frame, ~17 ms on Hyprland).
//!
//! `ext_image_copy_capture_manager_v1` would be the better protocol for
//! continuously animating glass (damage-driven frames, no per-request round
//! trip), but a static bar does one capture per change, which screencopy does
//! with far less code.

use std::fs::File;
use std::io;
use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use memmap2::MmapMut;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

/// Longest a single capture may take before we give up and keep the last frame.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(2);

/// A rectangle in output-local logical (surface) coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    /// Left edge.
    pub x: i32,
    /// Top edge.
    pub y: i32,
    /// Width in logical pixels.
    pub width: i32,
    /// Height in logical pixels.
    pub height: i32,
}

impl Region {
    /// Creates a region.
    pub const fn new(x: i32, y: i32, width: i32, height: i32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

/// A captured backdrop, sRGB-encoded RGBA8 with straight alpha.
#[derive(Debug, Clone)]
pub struct Captured {
    /// Width in physical pixels.
    pub width: u32,
    /// Height in physical pixels.
    pub height: u32,
    /// Tightly packed RGBA8 pixels.
    pub rgba: Vec<u8>,
    /// Wall time the compositor round trip took.
    pub latency_ms: f64,
}

/// Clips a region to an output of `bounds` logical size, or `None` if empty.
///
/// Region capture coordinates are output-local, so a bar with a large margin on
/// a small output can ask for pixels that do not exist; asking the compositor
/// for those makes the whole capture fail, so clip first.
pub fn clip_region(region: Region, bounds: (i32, i32)) -> Option<Region> {
    let x0 = region.x.max(0);
    let y0 = region.y.max(0);
    let x1 = (region.x + region.width).min(bounds.0.max(0));
    let y1 = (region.y + region.height).min(bounds.1.max(0));

    if x1 <= x0 || y1 <= y0 {
        return None;
    }

    Some(Region::new(x0, y0, x1 - x0, y1 - y0))
}

/// Picks the index of the output to capture from, by connector name.
///
/// Falls back to the first output when the requested name is unknown, which is
/// what a single-output compositor with an unexpected connector name needs.
pub fn pick_output(names: &[String], wanted: Option<&str>) -> Option<usize> {
    if names.is_empty() {
        return None;
    }

    wanted
        .and_then(|wanted| names.iter().position(|name| name == wanted))
        .or(Some(0))
}

/// Converts a little-endian `XRGB8888` shm buffer to tightly packed RGBA8.
pub fn xrgb_to_rgba(source: &[u8], width: u32, height: u32, stride: u32) -> Vec<u8> {
    let (width, height, stride) = (width as usize, height as usize, stride as usize);
    let mut rgba = Vec::with_capacity(width * height * 4);

    for y in 0..height {
        let row = &source[y * stride..];
        for x in 0..width {
            let px = &row[x * 4..x * 4 + 4];
            rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
        }
    }

    rgba
}

/// Wayland-side state for a capture connection.
struct State {
    shm: wl_shm::WlShm,
    pool: Option<(wl_shm_pool::WlShmPool, MmapMut, usize)>,
    outputs: Vec<(String, wl_output::WlOutput)>,
    frame: Option<ZwlrScreencopyFrameV1>,
    buffer: Option<wl_buffer::WlBuffer>,
    spec: Option<(u32, u32, u32)>,
    ready: bool,
    failed: bool,
}

impl State {
    /// Connector names of every bound output, in binding order.
    fn output_names(&self) -> Vec<String> {
        self.outputs.iter().map(|(name, _)| name.clone()).collect()
    }

    /// Grows (or creates) the shm pool so it can hold `size` bytes.
    fn reserve(&mut self, qh: &QueueHandle<Self>, size: usize) -> io::Result<()> {
        let size = size.next_multiple_of(4096);
        if let Some((_, mmap, len)) = &self.pool
            && *len >= size
        {
            let _ = mmap;
            return Ok(());
        }

        self.destroy_pool();

        let fd = rustix::fs::memfd_create("ironbar-glass", rustix::fs::MemfdFlags::CLOEXEC)
            .map_err(io::Error::from)?;
        rustix::fs::ftruncate(&fd, size as u64).map_err(io::Error::from)?;

        let file = File::from(fd);
        let pool = self.shm.create_pool(file.as_fd(), size as i32, qh, ());
        // Safety: the fd is an anonymous memfd we own and have sized ourselves.
        let mmap = unsafe { MmapMut::map_mut(&file) }?;

        self.pool = Some((pool, mmap, size));
        Ok(())
    }

    fn destroy_pool(&mut self) {
        if let Some((pool, _, _)) = self.pool.take() {
            pool.destroy();
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
            && interface == wl_output::WlOutput::interface().name
        {
            let output = registry.bind::<wl_output::WlOutput, _, _>(name, version.min(4), qh, ());
            state.outputs.push((String::new(), output));
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        output: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event
            && let Some((slot, _)) = state
                .outputs
                .iter_mut()
                .find(|(_, candidate)| candidate == output)
        {
            *slot = name;
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format,
                width,
                height,
                stride,
            } => {
                let Ok(format) = format.into_result() else {
                    state.failed = true;
                    return;
                };

                let size = stride as usize * height as usize;
                if let Err(err) = state.reserve(qh, size) {
                    tracing::error!("glass: could not create shm pool: {err}");
                    state.failed = true;
                    return;
                }

                let (pool, _, _) = state.pool.as_mut().expect("pool was just reserved");
                let buffer = pool.create_buffer(
                    0,
                    width as i32,
                    height as i32,
                    stride as i32,
                    format,
                    qh,
                    (),
                );

                if let Some(frame) = &state.frame {
                    frame.copy(&buffer);
                }

                state.buffer = Some(buffer);
                state.spec = Some((width, height, stride));
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => state.ready = true,
            zwlr_screencopy_frame_v1::Event::Failed => state.failed = true,
            _ => {}
        }
    }
}

delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: ignore ZwlrScreencopyManagerV1);

/// An open capture connection, tied to the thread that created it.
pub struct Capture {
    queue: EventQueue<State>,
    state: State,
    manager: ZwlrScreencopyManagerV1,
}

impl Capture {
    /// Connects to the compositor and binds the screencopy manager.
    pub fn connect() -> Result<Self, String> {
        let conn =
            Connection::connect_to_env().map_err(|err| format!("no wayland connection: {err}"))?;
        let (globals, mut queue) = registry_queue_init::<State>(&conn)
            .map_err(|err| format!("could not read registry: {err}"))?;
        let qh = queue.handle();

        let shm: wl_shm::WlShm = globals
            .bind(&qh, 1..=1, ())
            .map_err(|err| format!("wl_shm unavailable: {err}"))?;
        let manager: ZwlrScreencopyManagerV1 = globals
            .bind(&qh, 1..=3, ())
            .map_err(|err| format!("zwlr_screencopy_manager_v1 unavailable: {err}"))?;

        // Bind every wl_output (v4 gives us the connector name) so the bar can
        // capture the monitor it was placed on, not just the first one. The
        // dedicated registry below is why this needs its own `WlRegistry` data
        // type: each global must be bound by its registry name.
        let _registry = conn.display().get_registry(&qh, ());

        let mut state = State {
            shm,
            pool: None,
            outputs: Vec::new(),
            frame: None,
            buffer: None,
            spec: None,
            ready: false,
            failed: false,
        };

        // Wait for the globals and for wl_output events, so `pick_output` has
        // real connector names.
        let _ = queue.roundtrip(&mut state);

        Ok(Self {
            queue,
            state,
            manager,
        })
    }

    /// Captures `region` from the output named `output`.
    ///
    /// `region` must already be clipped to the output; see [`clip_region`].
    pub fn capture(&mut self, output: Option<&str>, region: Region) -> Result<Captured, String> {
        let index = pick_output(&self.state.output_names(), output)
            .ok_or_else(|| "no wl_output to capture".to_owned())?;
        let output_proxy = self.state.outputs[index].1.clone();

        self.state.ready = false;
        self.state.failed = false;
        self.state.spec = None;
        self.state.buffer = None;

        let started = Instant::now();
        let frame = self.manager.capture_output_region(
            0,
            &output_proxy,
            region.x,
            region.y,
            region.width,
            region.height,
            &self.queue.handle(),
            (),
        );
        self.state.frame = Some(frame);

        while !self.state.ready && !self.state.failed {
            if started.elapsed() > CAPTURE_TIMEOUT {
                let _ = self.state.frame.take().map(|frame| frame.destroy());
                return Err("screencopy timed out".to_owned());
            }

            self.queue
                .flush()
                .map_err(|err| format!("flush failed: {err}"))?;
            self.queue
                .blocking_dispatch(&mut self.state)
                .map_err(|err| format!("dispatch failed: {err}"))?;
        }

        let latency_ms = started.elapsed().as_secs_f64() * 1000.0;

        if self.state.failed {
            let _ = self.state.frame.take().map(|frame| frame.destroy());
            return Err("screencopy failed (region outside output?)".to_owned());
        }

        let (width, height, stride) = self
            .state
            .spec
            .ok_or_else(|| "screencopy sent no buffer description".to_owned())?;

        let (_, mmap, _) = self.state.pool.as_ref().expect("pool exists with a buffer");
        let rgba = xrgb_to_rgba(mmap, width, height, stride);

        if let Some(buffer) = self.state.buffer.take() {
            buffer.destroy();
        }
        if let Some(frame) = self.state.frame.take() {
            frame.destroy();
        }

        Ok(Captured {
            width,
            height,
            rgba,
            latency_ms,
        })
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        if let Some(buffer) = self.state.buffer.take() {
            buffer.destroy();
        }
        if let Some(frame) = self.state.frame.take() {
            frame.destroy();
        }
        self.state.destroy_pool();
        self.manager.destroy();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipping_stays_inside_the_output() {
        let region = Region::new(10, 10, 100, 40);
        assert_eq!(clip_region(region, (1920, 1080)), Some(region));

        let overhanging = Region::new(1900, 1070, 100, 40);
        assert_eq!(
            clip_region(overhanging, (1920, 1080)),
            Some(Region::new(1900, 1070, 20, 10))
        );

        let negative = Region::new(-5, -5, 20, 20);
        assert_eq!(
            clip_region(negative, (1920, 1080)),
            Some(Region::new(0, 0, 15, 15))
        );
    }

    #[test]
    fn clipping_rejects_empty_or_offscreen_regions() {
        assert_eq!(
            clip_region(Region::new(2000, 0, 100, 40), (1920, 1080)),
            None
        );
        assert_eq!(clip_region(Region::new(0, 0, 0, 40), (1920, 1080)), None);
        assert_eq!(clip_region(Region::new(-50, 0, 40, 40), (1920, 1080)), None);
    }

    #[test]
    fn output_selection_prefers_the_requested_name() {
        let names = vec!["eDP-1".to_owned(), "DP-1".to_owned()];

        assert_eq!(pick_output(&names, Some("DP-1")), Some(1));
        assert_eq!(pick_output(&names, Some("HDMI-A-1")), Some(0));
        assert_eq!(pick_output(&names, None), Some(0));
        assert_eq!(pick_output(&[], Some("eDP-1")), None);
    }

    #[test]
    fn xrgb_is_reordered_to_rgba() {
        // Two pixels: blue then red, stored little-endian as B,G,R,X.
        let source = [0xFF, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF, 0x00];
        assert_eq!(
            xrgb_to_rgba(&source, 2, 1, 8),
            vec![0, 0, 255, 255, 255, 0, 0, 255]
        );
    }

    #[test]
    fn xrgb_respects_row_stride() {
        // 1 pixel wide, stride of 8: the padding must not leak into output.
        let source = [0x10, 0x20, 0x30, 0x00, 0xAA, 0xAA, 0xAA, 0xAA];
        assert_eq!(xrgb_to_rgba(&source, 1, 1, 8), vec![0x30, 0x20, 0x10, 255]);
    }
}
