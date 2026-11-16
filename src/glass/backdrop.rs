//! The GTK side of the glass backdrop: a `gtk::DrawingArea` that paints the
//! rendered glass under the bar content, plus the worker thread that owns
//! capture + wgpu.
//!
//! The worker is a plain OS thread with its own wayland connection: a region
//! capture is a blocking compositor round trip (~17 ms measured on Hyprland) and
//! the GPU→CPU readback is a pipeline stall, so neither may run on the GTK main
//! loop. Everything it produces is plain bytes; the `gdk::Texture` is built on
//! the main thread.

use std::cell::RefCell;
use std::io::{BufRead, BufReader};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use gtk::gdk;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use tokio::sync::mpsc as tokio_mpsc;
use tracing::{debug, error, info, warn};

use crate::channels::MpscReceiverExt;
use crate::config::{BarPosition, MarginConfig};
use crate::modules::ModuleRef;

use super::capture::{Capture, Captured, Region, clip_region};
use super::config::{GlassConfig, GlassRefresh};
use super::render::{GlassRenderer, Pill};

/// How long to wait after a compositor event before capturing again, so a burst
/// of window moves produces one capture instead of a dozen.
const DEBOUNCE: Duration = Duration::from_millis(150);

/// Interval used for `on-change` when no compositor event stream is available.
const FALLBACK_INTERVAL: Duration = Duration::from_millis(1000);

/// Where the bar is on screen, in the terms capture needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GlassTarget {
    /// Region to capture, in output-local logical pixels.
    pub region: Region,
    /// Logical size of the output, used to clip the region.
    pub bounds: (i32, i32),
}

/// Works out the region of the screen covered by a bar.
///
/// `monitor` is the monitor geometry in global logical coordinates (GDK's
/// `Monitor::geometry`), `bar` is the allocated size of the bar window, and the
/// result is clipped to the monitor it sits on.
#[allow(clippy::too_many_arguments)]
pub fn bar_target(
    monitor: (i32, i32, i32, i32),
    bar: (i32, i32),
    position: BarPosition,
    margin: MarginConfig,
    anchor_to_edges: bool,
) -> Option<GlassTarget> {
    let (mon_x, mon_y, mon_w, mon_h) = monitor;
    let (bar_w, bar_h) = bar;

    if bar_w <= 0 || bar_h <= 0 {
        return None;
    }

    let horizontal = matches!(position, BarPosition::Top | BarPosition::Bottom);

    let (x, y) = if horizontal {
        let x = if anchor_to_edges {
            mon_x + margin.left
        } else {
            mon_x + (mon_w - bar_w) / 2
        };
        let y = match position {
            BarPosition::Top => mon_y + margin.top,
            _ => mon_y + mon_h - margin.bottom - bar_h,
        };
        (x, y)
    } else {
        let y = if anchor_to_edges {
            mon_y + margin.top
        } else {
            mon_y + (mon_h - bar_h) / 2
        };
        let x = match position {
            BarPosition::Left => mon_x + margin.left,
            _ => mon_x + mon_w - margin.right - bar_w,
        };
        (x, y)
    };

    let region = Region::new(x - mon_x, y - mon_y, bar_w, bar_h);
    let region = clip_region(region, (mon_w, mon_h))?;

    Some(GlassTarget {
        region,
        bounds: (mon_w, mon_h),
    })
}

/// Converts the live widget allocations of the bar's modules into pill
/// rectangles, in physical pixels relative to the bar.
pub fn module_pills(content: &gtk::CenterBox, modules: &[ModuleRef], scale: f64) -> Vec<Pill> {
    modules
        .iter()
        .filter_map(|module| {
            let root = &module.root_widget;
            let bounds = root.compute_bounds(content)?;
            let pill = Pill {
                x: (bounds.x() as f64 * scale) as f32,
                y: (bounds.y() as f64 * scale) as f32,
                width: (bounds.width() as f64 * scale) as f32,
                height: (bounds.height() as f64 * scale) as f32,
            };

            (pill.width >= 2.0 && pill.height >= 2.0).then_some(pill)
        })
        .collect()
}

/// Worker request.
enum Request {
    /// A new geometry, from a map or size change.
    Update {
        target: GlassTarget,
        size: (u32, u32),
        scale: f32,
        pills: Vec<Pill>,
    },
    /// Re-capture at the current geometry.
    Refresh,
    /// Stop the worker.
    Stop,
}

/// Worker reply.
enum Reply {
    /// A rendered frame, RGBA8 in physical pixels.
    Frame {
        rgba: Vec<u8>,
        width: u32,
        height: u32,
        stats: Stats,
    },
    /// The worker cannot render at all; fall back to CSS permanently.
    Fatal(String),
}

#[derive(Debug, Clone, Copy)]
struct Stats {
    capture_ms: f64,
    draw_ms: f64,
    upload_ms: f64,
    readback_ms: f64,
}

impl std::fmt::Display for Stats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "capture {:.1} ms, glass {:.2} ms, upload {:.2} ms, readback {:.2} ms",
            self.capture_ms, self.draw_ms, self.upload_ms, self.readback_ms
        )
    }
}

/// The widget that paints a glass frame under the bar content.
///
/// A `gtk::DrawingArea` cannot be used here: its draw function hands out a
/// cairo context, and cairo has no way to blit a `gdk::Texture`. A widget with a
/// `snapshot()` override draws the texture with GSK directly, and measuring to
/// zero keeps it from influencing the bar's layout.
mod imp {
    use super::*;

    #[derive(Debug, Default)]
    pub struct GlassSurface {
        pub texture: RefCell<Option<gdk::Texture>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for GlassSurface {
        const NAME: &'static str = "IronbarGlassSurface";
        type Type = super::GlassSurface;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for GlassSurface {
        fn constructed(&self) {
            self.parent_constructed();

            let widget = self.obj();
            widget.set_hexpand(true);
            widget.set_vexpand(true);
            widget.set_can_target(false);
            widget.add_css_class("liquid-glass");
        }
    }

    impl WidgetImpl for GlassSurface {
        fn measure(&self, _orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            (0, 0, -1, -1)
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let widget = self.obj();
            let Some(texture) = self.texture.borrow().clone() else {
                return;
            };

            let bounds =
                gtk::graphene::Rect::new(0.0, 0.0, widget.width() as f32, widget.height() as f32);
            snapshot.append_texture(&texture, &bounds);
        }
    }
}

glib::wrapper! {
    /// Zero-sized layout-wise surface that paints the glass backdrop.
    pub struct GlassSurface(ObjectSubclass<imp::GlassSurface>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl GlassSurface {
    fn new() -> Self {
        glib::Object::new()
    }

    fn set_texture(&self, texture: gdk::Texture) {
        *self.imp().texture.borrow_mut() = Some(texture);
        self.queue_draw();
    }
}

/// A glass backdrop attached to a bar widget tree.
#[derive(Debug)]
pub struct GlassBackdrop {
    area: GlassSurface,
    sender: mpsc::Sender<Request>,
    scale: f64,
    pills_enabled: bool,
}

impl GlassBackdrop {
    /// Builds the glass widget and spawns its worker thread.
    ///
    /// Returns `None` when the effect is disabled in config. A worker that later
    /// fails to capture or render reports [`Reply::Fatal`] and the widget stays
    /// transparent, which leaves the bar's CSS background as the visible look.
    pub fn new(config: &GlassConfig, output: Option<String>, scale: f64) -> Option<Self> {
        if !config.is_enabled() {
            return None;
        }

        let first_frame = Rc::new(std::cell::Cell::new(true));
        let area = GlassSurface::new();

        let (tx, rx) = mpsc::channel::<Request>();
        let (reply_tx, reply_rx) = tokio_mpsc::channel::<Reply>(4);

        let refresh = config.refresh;

        // Capture-on-change needs a compositor event stream; without one the
        // worker polls instead, which is honest but costs one round trip a
        // second on the capture thread.
        let listening = refresh == GlassRefresh::OnChange && spawn_compositor_events(tx.clone());
        if refresh == GlassRefresh::OnChange && !listening {
            info!(
                "glass: no compositor event stream, refreshing every {} ms instead",
                FALLBACK_INTERVAL.as_millis()
            );
        }

        let interval = match refresh {
            GlassRefresh::Interval(ms) => Some(Duration::from_millis(ms)),
            GlassRefresh::OnChange => listening.then_some(FALLBACK_INTERVAL),
            GlassRefresh::Never => None,
        };

        let pills_enabled = config.pills;
        let worker_config = config.clone();
        let output_name = output;

        std::thread::Builder::new()
            .name("ironbar-glass".into())
            .spawn(move || {
                if let Err(err) = worker(worker_config, output_name, interval, rx, reply_tx.clone())
                {
                    let _ = reply_tx.blocking_send(Reply::Fatal(err));
                }
            })
            .map_err(|err| warn!("glass: could not spawn render thread: {err}"))
            .ok()?;

        {
            let first_frame = first_frame.clone();
            reply_rx.recv_glib(&area, move |surface, reply| match reply {
                Reply::Frame {
                    rgba,
                    width,
                    height,
                    stats,
                } => {
                    let bytes = glib::Bytes::from_owned(rgba);
                    let stride = 4 * width as usize;
                    let texture = gdk::MemoryTexture::new(
                        width as i32,
                        height as i32,
                        gdk::MemoryFormat::R8g8b8a8,
                        &bytes,
                        stride,
                    );

                    let first = first_frame.replace(false);

                    surface.set_texture(texture.upcast());

                    if first {
                        info!("glass: first backdrop rendered ({stats})");

                        // Only now is the glass visible, so only now may the CSS
                        // background be cleared: until this point the bar must
                        // keep looking exactly like a non-glass build.
                        if let Some(window) = surface
                            .root()
                            .and_then(|root| root.downcast::<gtk::Window>().ok())
                        {
                            window.add_css_class("liquid-glass-active");
                        }
                        crate::style::set_glass_background_transparent();
                    } else {
                        debug!("glass: {stats}");
                    }
                }
                Reply::Fatal(err) => {
                    // One warning, then the bar keeps its CSS background: the
                    // glass widget stays transparent, so nothing else changes.
                    warn!("glass: falling back to plain CSS: {err}");
                }
            });
        }

        Some(Self {
            area,
            sender: tx,
            scale,
            pills_enabled,
        })
    }

    /// The widget to install behind the bar content.
    pub fn widget(&self) -> &GlassSurface {
        &self.area
    }

    /// Triggers one re-capture with fresh module pill rectangles.
    pub fn refresh(&self, target: GlassTarget, size: (u32, u32), pills: Vec<Pill>) {
        let pills = if self.pills_enabled {
            pills
        } else {
            Vec::new()
        };
        let _ = self.sender.send(Request::Update {
            target,
            size,
            scale: self.scale as f32,
            pills,
        });
    }
}

impl Drop for GlassBackdrop {
    fn drop(&mut self) {
        let _ = self.sender.send(Request::Stop);
    }
}

/// Runs capture + render until the channel closes.
fn worker(
    config: GlassConfig,
    output: Option<String>,
    interval: Option<Duration>,
    rx: mpsc::Receiver<Request>,
    replies: tokio_mpsc::Sender<Reply>,
) -> Result<(), String> {
    let mut capture = Capture::connect()?;
    let mut renderer = GlassRenderer::new(&config)?;

    info!(
        "glass: rendering on {} (capture output: {})",
        renderer.adapter(),
        output.as_deref().unwrap_or("<first>")
    );

    let mut job: Option<(GlassTarget, (u32, u32), f32)> = None;
    let mut pills: Vec<Pill> = Vec::new();
    let mut due = Instant::now();

    loop {
        let request = match interval {
            Some(_) => {
                let timeout = due.saturating_duration_since(Instant::now());
                match rx.recv_timeout(timeout.max(Duration::from_millis(1))) {
                    Ok(request) => Some(request),
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            None => match rx.recv() {
                Ok(request) => Some(request),
                Err(_) => break,
            },
        };

        let mut force = false;
        match request {
            Some(Request::Update {
                target,
                size,
                scale,
                pills: new_pills,
            }) => {
                job = Some((target, size, scale));
                pills = new_pills;
                force = true;
            }
            Some(Request::Refresh) => force = true,
            Some(Request::Stop) => break,
            None => {}
        }

        if force {
            // Coalesce anything that piled up while we were busy.
            while let Ok(pending) = rx.try_recv() {
                match pending {
                    Request::Update {
                        target,
                        size,
                        scale,
                        pills: new_pills,
                    } => {
                        job = Some((target, size, scale));
                        pills = new_pills;
                    }
                    Request::Refresh => {}
                    Request::Stop => return Ok(()),
                }
            }

            let Some((target, (width, height), scale)) = job else {
                continue;
            };

            match render_frame(
                &mut capture,
                &mut renderer,
                output.as_deref(),
                target,
                (width, height),
                scale,
                &pills,
                &config,
            ) {
                Ok((captured, rendered)) => {
                    let stats = Stats {
                        capture_ms: captured.latency_ms,
                        draw_ms: rendered.draw_ms,
                        upload_ms: rendered.upload_ms,
                        readback_ms: rendered.readback_ms,
                    };

                    if replies
                        .blocking_send(Reply::Frame {
                            rgba: rendered.rgba,
                            width: captured.width,
                            height: captured.height,
                            stats,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                Err(err) => {
                    warn!("glass: {err}");
                }
            }
        }

        if let Some(interval) = interval {
            due = Instant::now() + interval;
        }
    }

    Ok(())
}

/// One capture + render pair, with the bar's radius applied.
#[allow(clippy::too_many_arguments)]
fn render_frame(
    capture: &mut Capture,
    renderer: &mut GlassRenderer,
    output: Option<&str>,
    target: GlassTarget,
    size: (u32, u32),
    scale: f32,
    pills: &[Pill],
    config: &GlassConfig,
) -> Result<(Captured, super::render::Rendered), String> {
    let captured = capture.capture(output, target.region)?;

    if captured.width != size.0 || captured.height != size.1 {
        debug!(
            "glass: capture is {}x{} but the widget is {}x{} (scale {scale})",
            captured.width, captured.height, size.0, size.1
        );
    }

    let radius = config.radius_px(size.1);
    renderer.set_layout(radius, pills.to_vec());

    let rendered = renderer.render(&captured.rgba, size, scale)?;

    Ok((captured, rendered))
}

/// Subscribes to Hyprland's event socket, returning false when unavailable.
fn spawn_compositor_events(sender: mpsc::Sender<Request>) -> bool {
    let Ok(signature) = std::env::var("HYPRLAND_INSTANCE_SIGNATURE") else {
        return false;
    };

    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_owned());
    let path = format!("{runtime_dir}/hypr/{signature}/.socket2.sock");

    if std::os::unix::net::UnixStream::connect(&path).is_err() {
        return false;
    }

    std::thread::Builder::new()
        .name("ironbar-glass-events".into())
        .spawn(move || {
            info!("glass: listening for compositor events on '{path}'");

            loop {
                let Ok(stream) = std::os::unix::net::UnixStream::connect(&path) else {
                    std::thread::sleep(Duration::from_secs(1));
                    continue;
                };

                let mut last_sent = Instant::now() - DEBOUNCE;
                for line in BufReader::new(stream).lines() {
                    let Ok(line) = line else { break };

                    let event = line.split(">>").next().unwrap_or_default();
                    if !is_backdrop_event(event) {
                        continue;
                    }

                    if last_sent.elapsed() >= DEBOUNCE {
                        last_sent = Instant::now();
                        if sender.send(Request::Refresh).is_err() {
                            return;
                        }
                    }
                }

                error!("glass: compositor event socket closed, reconnecting");
                std::thread::sleep(Duration::from_millis(250));
            }
        })
        .is_ok()
}

/// Whether a Hyprland event can change what is behind the bar.
fn is_backdrop_event(event: &str) -> bool {
    matches!(
        event,
        "openwindow"
            | "closewindow"
            | "movewindow"
            | "windowtitle"
            | "activewindow"
            | "workspace"
            | "focusedmon"
            | "createworkspace"
            | "destroyworkspace"
            | "moveworkspace"
            | "activespecial"
            | "togglespecialworkspace"
            | "fullscreen"
            | "changefloatingmode"
            | "monitoradded"
            | "monitorremoved"
            | "moveworkspacetomonitor"
            | "submap"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const MONITOR: (i32, i32, i32, i32) = (0, 0, 1920, 1080);

    fn margin(top: i32, bottom: i32, left: i32, right: i32) -> MarginConfig {
        MarginConfig {
            top,
            bottom,
            left,
            right,
        }
    }

    #[test]
    fn bottom_bar_region_sits_at_the_monitor_edge() {
        let target = bar_target(
            MONITOR,
            (1910, 38),
            BarPosition::Bottom,
            margin(0, 5, 5, 5),
            true,
        )
        .expect("region");

        assert_eq!(target.region, Region::new(5, 1080 - 5 - 38, 1910, 38));
        assert_eq!(target.bounds, (1920, 1080));
    }

    #[test]
    fn top_bar_region_starts_at_the_margin() {
        let target = bar_target(
            MONITOR,
            (1910, 38),
            BarPosition::Top,
            margin(5, 0, 5, 5),
            true,
        )
        .expect("region");

        assert_eq!(target.region, Region::new(5, 5, 1910, 38));
    }

    #[test]
    fn secondary_monitor_is_relative_to_its_own_origin() {
        let target = bar_target(
            (1920, 0, 2560, 1440),
            (2550, 40),
            BarPosition::Top,
            MarginConfig::default(),
            true,
        )
        .expect("region");

        assert_eq!(target.region, Region::new(0, 0, 2550, 40));
        assert_eq!(target.bounds, (2560, 1440));
    }

    #[test]
    fn unanchored_bar_is_centered() {
        let target = bar_target(
            MONITOR,
            (1000, 40),
            BarPosition::Bottom,
            MarginConfig::default(),
            false,
        )
        .expect("region");

        assert_eq!(target.region.x, 460);
        assert_eq!(target.region.y, 1040);
    }

    #[test]
    fn vertical_bars_use_the_full_height() {
        let target = bar_target(
            MONITOR,
            (42, 1080),
            BarPosition::Right,
            margin(0, 0, 0, 5),
            true,
        )
        .expect("region");

        assert_eq!(target.region, Region::new(1920 - 5 - 42, 0, 42, 1080));
    }

    #[test]
    fn empty_bar_has_no_region() {
        assert!(
            bar_target(
                MONITOR,
                (0, 0),
                BarPosition::Bottom,
                MarginConfig::default(),
                true
            )
            .is_none()
        );
    }

    #[test]
    fn oversized_margins_are_clipped_away() {
        let target = bar_target(
            MONITOR,
            (100, 40),
            BarPosition::Bottom,
            margin(0, 2000, 0, 0),
            true,
        );

        assert!(target.is_none(), "a bar fully off-screen must not capture");
    }

    #[test]
    fn only_material_events_refresh_the_backdrop() {
        assert!(is_backdrop_event("openwindow"));
        assert!(is_backdrop_event("workspace"));
        assert!(!is_backdrop_event("mouse"));
        assert!(!is_backdrop_event("tick"));
    }
}
