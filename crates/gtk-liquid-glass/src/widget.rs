//! The GTK side: a paint widget under the window content, the worker thread
//! that owns capture + wgpu, and the refresh policy.
//!
//! Threads talk to the GTK main loop through `MainContext::invoke` with a
//! `SendWeakRef` to the paint widget, so nothing here needs an async runtime
//! and a destroyed widget simply drops late replies.

use std::cell::{Cell, RefCell};
use std::io::{BufRead, BufReader};
use std::rc::{Rc, Weak};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use glib::SendWeakRef;
use gtk::gdk;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk_layer_shell::{Edge, LayerShell};
use tracing::{debug, info, warn};

use crate::capture::{Capture, Captured, Region};
use crate::locate::{self, Anchors, Margins};
use crate::render::{GlassRenderer, Pill, Rendered};
use crate::{GlassConfig, Refresh};

/// Debounce between a trigger (map, resize, compositor event) and a capture.
const DEBOUNCE: Duration = Duration::from_millis(150);
/// `on-change` interval when there is no compositor event stream.
const FALLBACK_INTERVAL: Duration = Duration::from_millis(1000);
/// How often pill rectangles are compared for a cheap re-render.
const PILL_POLL: Duration = Duration::from_millis(1000);
/// Frames to wait after hiding before capturing: one to draw the transparent
/// frame, one for the compositor to have presented it.
const HIDE_FRAMES: u32 = 2;
/// Safety net: content is always shown again after this long.
const HIDE_TIMEOUT: Duration = Duration::from_millis(1000);

/// Rectangles to render as pills, logical pixels relative to the overlay.
pub type PillProvider = Box<dyn Fn() -> Vec<gtk::graphene::Rect>>;

/// Callback run once the first glass frame is on screen.
pub type FirstFrame = Box<dyn Fn(&gtk::Widget)>;

/// A capture job, in output-local logical pixels.
#[derive(Debug, Clone)]
struct Job {
    output: Option<String>,
    /// Region to capture (window + lens padding).
    region: Region,
    /// The glass rectangle, relative to `region`.
    inner: Region,
    radius: f32,
    pills: Vec<gtk::graphene::Rect>,
}

enum Request {
    Capture(Job),
    /// Re-render the last capture with new pills; no compositor round trip.
    Relayout(Vec<gtk::graphene::Rect>),
    Stop,
}

/// Timings of one frame.
#[derive(Debug, Clone, Copy, Default)]
pub struct Stats {
    /// Compositor round trip.
    pub capture_ms: f64,
    /// Glass shader.
    pub draw_ms: f64,
    /// Backdrop upload.
    pub upload_ms: f64,
    /// GPU→CPU readback.
    pub readback_ms: f64,
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

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct GlassSurface {
        pub texture: RefCell<Option<gdk::Texture>>,
        pub controller: RefCell<Option<Rc<Inner>>>,
        pub last_size: Cell<(i32, i32)>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for GlassSurface {
        const NAME: &'static str = "GtkLiquidGlassSurface";
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

        fn dispose(&self) {
            self.controller.take();
        }
    }

    impl WidgetImpl for GlassSurface {
        fn measure(&self, _: gtk::Orientation, _: i32) -> (i32, i32, i32, i32) {
            (0, 0, -1, -1)
        }

        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            self.parent_size_allocate(width, height, baseline);
            if self.last_size.replace((width, height)) != (width, height)
                && let Some(inner) = self.controller.borrow().as_ref()
            {
                inner.schedule();
            }
        }

        fn map(&self) {
            self.parent_map();
            if let Some(inner) = self.controller.borrow().as_ref() {
                inner.schedule();
            }
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let widget = self.obj();
            if let Some(texture) = self.texture.borrow().as_ref() {
                let bounds = gtk::graphene::Rect::new(
                    0.0,
                    0.0,
                    widget.width() as f32,
                    widget.height() as f32,
                );
                snapshot.append_texture(texture, &bounds);
            }
        }
    }
}

glib::wrapper! {
    /// Layout-neutral widget that paints the glass texture.
    pub struct GlassSurface(ObjectSubclass<imp::GlassSurface>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

/// Shared state behind a [`GlassWidget`]; owned by the paint widget.
pub struct Inner {
    config: GlassConfig,
    overlay: glib::WeakRef<gtk::Overlay>,
    child: glib::WeakRef<gtk::Widget>,
    surface: glib::WeakRef<GlassSurface>,
    sender: mpsc::Sender<Request>,
    pills: RefCell<Option<PillProvider>>,
    last_pills: RefCell<Vec<gtk::graphene::Rect>>,
    first_frame: RefCell<Vec<FirstFrame>>,
    shown: Cell<bool>,
    scheduled: Cell<bool>,
    capturing: Cell<bool>,
    pending: Cell<bool>,
    hidden_at: Cell<Option<Instant>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        let _ = self.sender.send(Request::Stop);
    }
}

impl Inner {
    /// Debounced capture request.
    fn schedule(self: &Rc<Self>) {
        if self.scheduled.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(DEBOUNCE, move || {
            if let Some(inner) = weak.upgrade() {
                inner.scheduled.set(false);
                inner.start_capture();
            }
        });
    }

    fn pill_rects(&self) -> Vec<gtk::graphene::Rect> {
        self.pills
            .borrow()
            .as_ref()
            .map(|f| f())
            .unwrap_or_default()
    }

    /// Works out the job for the current geometry, or `None` if not placed yet.
    fn job(&self) -> Option<Job> {
        let overlay = self.overlay.upgrade()?;
        let window = overlay.root()?.downcast::<gtk::Window>().ok()?;
        if !window.is_mapped() || window.width() <= 0 || !overlay.is_mapped() {
            return None;
        }

        let (output, output_size, window_region) = window_placement(&window)?;

        // The surface the overlay is drawn in: the window itself, or a popover
        // (an xdg_popup positioned relative to the window's surface).
        let native = overlay.native()?;
        let (surface_region, native_widget): (Region, gtk::Widget) =
            match native.clone().downcast::<gtk::Popover>() {
                Ok(popover) => {
                    let popup = popover.surface()?.downcast::<gdk::Popup>().ok()?;
                    // The popup position is relative to the parent surface; the
                    // popover widget sits inside its own surface at the surface
                    // transform (shadow / CSS margins).
                    let (sx, sy) = popover.surface_transform();
                    let region = Region::new(
                        window_region.x + popup.position_x() + sx.round() as i32,
                        window_region.y + popup.position_y() + sy.round() as i32,
                        popover.width(),
                        popover.height(),
                    );
                    (region, popover.upcast())
                }
                Err(_) => (window_region, window.clone().upcast()),
            };

        // The glass covers the overlay, which may be inset in the window.
        let bounds = overlay.compute_bounds(&native_widget)?;
        let region = Region::new(
            surface_region.x + bounds.x().round() as i32,
            surface_region.y + bounds.y().round() as i32,
            bounds.width().round() as i32,
            bounds.height().round() as i32,
        );
        let (padded, (ox, oy)) =
            locate::pad_region(region, self.config.lens_padding.max(0), output_size)?;

        debug!("glass: surface at {surface_region:?}, capturing {padded:?}");

        Some(Job {
            output,
            region: padded,
            inner: Region::new(
                ox,
                oy,
                region.width.min(padded.width - ox),
                region.height.min(padded.height - oy),
            ),
            radius: self.config.radius_for(region.height as f32),
            pills: self.pill_rects(),
        })
    }

    fn start_capture(self: &Rc<Self>) {
        if self.capturing.get() {
            self.pending.set(true);
            return;
        }
        let Some(job) = self.job() else {
            return;
        };
        *self.last_pills.borrow_mut() = job.pills.clone();

        let Some(overlay) = self.overlay.upgrade() else {
            return;
        };
        self.capturing.set(true);

        if !self.config.hide_during_capture {
            let _ = self.sender.send(Request::Capture(job));
            return;
        }

        // Hide our own pixels, wait until the compositor has presented the
        // transparent frame, then capture. See the crate docs ("self-capture").
        overlay.set_opacity(0.0);
        self.hidden_at.set(Some(Instant::now()));
        let frames = Cell::new(0u32);
        let sender = self.sender.clone();
        let job = RefCell::new(Some(job));
        overlay.add_tick_callback(move |_, _| {
            frames.set(frames.get() + 1);
            if frames.get() < HIDE_FRAMES {
                return glib::ControlFlow::Continue;
            }
            if let Some(job) = job.take() {
                let _ = sender.send(Request::Capture(job));
            }
            glib::ControlFlow::Break
        });

        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(HIDE_TIMEOUT, move || {
            if let Some(inner) = weak.upgrade()
                && inner.capturing.get()
            {
                inner.captured();
            }
        });
    }

    /// The compositor has our backdrop: show the content again.
    fn captured(self: &Rc<Self>) {
        if let Some(overlay) = self.overlay.upgrade() {
            overlay.set_opacity(1.0);
        }
        if let Some(hidden) = self.hidden_at.take() {
            debug!(
                "glass: content hidden for {:.1} ms during capture",
                hidden.elapsed().as_secs_f64() * 1000.0
            );
        }
        if self.capturing.replace(false) && self.pending.replace(false) {
            self.schedule();
        }
    }

    fn frame(&self, rgba: Vec<u8>, width: u32, height: u32, stats: Stats) {
        let Some(surface) = self.surface.upgrade() else {
            return;
        };
        let bytes = glib::Bytes::from_owned(rgba);
        let texture = gdk::MemoryTexture::new(
            width as i32,
            height as i32,
            gdk::MemoryFormat::R8g8b8a8,
            &bytes,
            4 * width as usize,
        );
        *surface.imp().texture.borrow_mut() = Some(texture.upcast());
        surface.queue_draw();

        if self.shown.replace(true) {
            debug!("glass: {stats}");
        } else {
            info!("glass: first frame {width}x{height} ({stats})");
            if let Some(child) = self.child.upgrade() {
                for callback in self.first_frame.borrow().iter() {
                    callback(&child);
                }
            }
        }
    }

    fn poll_pills(&self) {
        if self.pills.borrow().is_none() || !self.shown.get() {
            return;
        }
        let pills = self.pill_rects();
        if *self.last_pills.borrow() != pills {
            *self.last_pills.borrow_mut() = pills.clone();
            let _ = self.sender.send(Request::Relayout(pills));
        }
    }
}

/// Output name, output logical size and output-local region of a window.
///
/// Uses the compositor's view (Hyprland `j/layers`) when available and the
/// layer-shell anchors/margins otherwise; see [`locate`].
fn window_placement(window: &gtk::Window) -> Option<(Option<String>, (i32, i32), Region)> {
    let layer = window.is_layer_window();
    let monitor = layer
        .then(|| LayerShell::monitor(window))
        .flatten()
        .or_else(|| {
            let surface = window.surface()?;
            gtk::prelude::WidgetExt::display(window).monitor_at_surface(&surface)
        })?;
    let geometry = monitor.geometry();
    let output_size = (geometry.width(), geometry.height());
    let window_size = (window.width(), window.height());
    let output = monitor.connector().map(String::from);

    let (anchors, margins) = if layer {
        (
            Anchors {
                top: window.is_anchor(Edge::Top),
                bottom: window.is_anchor(Edge::Bottom),
                left: window.is_anchor(Edge::Left),
                right: window.is_anchor(Edge::Right),
            },
            Margins {
                top: window.margin(Edge::Top),
                bottom: window.margin(Edge::Bottom),
                left: window.margin(Edge::Left),
                right: window.margin(Edge::Right),
            },
        )
    } else {
        (Anchors::default(), Margins::default())
    };

    let guess = locate::layer_region(output_size, window_size, anchors, margins);
    let compositor = if layer {
        let namespace = window
            .namespace()
            .map(|ns| ns.to_string())
            .unwrap_or_default();
        output.as_deref().and_then(|output| {
            let json = locate::hyprland_request("j/layers")?;
            locate::hyprland_layer(
                &json,
                output,
                std::process::id(),
                &namespace,
                window_size,
                (geometry.x(), geometry.y()),
                guess,
            )
        })
    } else {
        None
    };

    debug!(
        "glass: window placed from {}",
        if compositor.is_some() {
            "compositor"
        } else {
            "layer-shell math"
        }
    );
    Some((output, output_size, compositor.or(guess)?))
}

/// Handle to a glass backdrop installed under a widget.
#[derive(Clone)]
pub struct GlassWidget {
    inner: Rc<Inner>,
    // Strong: the paint widget owns `Inner`, so `Inner` may only hold the
    // overlay weakly; the handle (or the overlay's parent) keeps it alive.
    overlay: gtk::Overlay,
}

impl std::fmt::Debug for GlassWidget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GlassWidget")
            .field("config", &self.inner.config)
            .finish_non_exhaustive()
    }
}

impl GlassWidget {
    /// Wraps `child` in a `gtk::Overlay` with a glass layer under it.
    ///
    /// Put [`GlassWidget::overlay`] where `child` used to be. Nothing is
    /// captured until the window is mapped; the glass layer stays transparent
    /// until the first frame, so a failure leaves the window looking unchanged.
    pub fn new(child: &impl IsA<gtk::Widget>, config: &GlassConfig) -> Self {
        let surface: GlassSurface = glib::Object::new();
        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&surface));
        overlay.add_overlay(child);
        // An overlay measures its main child only, and the paint widget is
        // zero-sized: without this the window falls back to GTK's 200 px
        // default and the stretched glass shows up as a ghost under the bar.
        overlay.set_measure_overlay(child, true);
        overlay.add_css_class("liquid-glass-overlay");

        let (sender, requests) = mpsc::channel();
        let inner = Rc::new(Inner {
            config: config.clone(),
            overlay: overlay.downgrade(),
            child: child.as_ref().downgrade(),
            surface: surface.downgrade(),
            sender,
            pills: RefCell::new(None),
            last_pills: RefCell::new(Vec::new()),
            first_frame: RefCell::new(Vec::new()),
            shown: Cell::new(false),
            scheduled: Cell::new(false),
            capturing: Cell::new(false),
            pending: Cell::new(false),
            hidden_at: Cell::new(None),
        });
        *surface.imp().controller.borrow_mut() = Some(inner.clone());

        spawn_worker(
            config.clone(),
            requests,
            SendWeakRef::from(surface.downgrade()),
        );
        install_refresh(&inner, &surface);

        Self { inner, overlay }
    }

    /// The overlay holding the glass layer and the child.
    pub fn overlay(&self) -> gtk::Overlay {
        self.overlay.clone()
    }

    /// Re-captures and re-renders as soon as possible (debounced).
    pub fn refresh(&self) {
        self.inner.schedule();
    }

    /// Supplies pill rectangles (logical, relative to the overlay). They are
    /// read on every capture and compared once a second; a change re-renders
    /// the last backdrop without a new capture.
    pub fn set_pill_provider(&self, provider: impl Fn() -> Vec<gtk::graphene::Rect> + 'static) {
        *self.inner.pills.borrow_mut() = Some(Box::new(provider));
    }

    /// Runs `f` with the child once the first glass frame is on screen, e.g. to
    /// drop a CSS background that was kept as the fallback look.
    pub fn connect_first_frame(&self, f: impl Fn(&gtk::Widget) + 'static) {
        self.inner.first_frame.borrow_mut().push(Box::new(f));
    }
}

/// Wraps `child` in an overlay with a liquid glass backdrop of the region
/// behind its layer-shell window, captured on map and refreshed per
/// `config.refresh`. The glass lives as long as the returned overlay.
pub fn glass_overlay(child: &impl IsA<gtk::Widget>, config: &GlassConfig) -> gtk::Overlay {
    GlassWidget::new(child, config).overlay()
}

fn install_refresh(inner: &Rc<Inner>, surface: &GlassSurface) {
    let interval = match inner.config.refresh {
        Refresh::Never => None,
        Refresh::Interval(ms) => Some(Duration::from_millis(ms.max(100))),
        Refresh::OnChange => {
            if spawn_compositor_events(SendWeakRef::from(surface.downgrade())) {
                None
            } else {
                info!(
                    "glass: no compositor event stream, refreshing every {} ms",
                    FALLBACK_INTERVAL.as_millis()
                );
                Some(FALLBACK_INTERVAL)
            }
        }
    };

    let weak: Weak<Inner> = Rc::downgrade(inner);
    if let Some(interval) = interval {
        let weak = weak.clone();
        glib::timeout_add_local(interval, move || match weak.upgrade() {
            Some(inner) => {
                inner.schedule();
                glib::ControlFlow::Continue
            }
            None => glib::ControlFlow::Break,
        });
    }

    glib::timeout_add_local(PILL_POLL, move || match weak.upgrade() {
        Some(inner) => {
            inner.poll_pills();
            glib::ControlFlow::Continue
        }
        None => glib::ControlFlow::Break,
    });
}

/// Runs `f` on the main loop with the surface's controller, if still alive.
fn on_main(surface: &SendWeakRef<GlassSurface>, f: impl FnOnce(&Rc<Inner>) + Send + 'static) {
    let surface = surface.clone();
    glib::MainContext::default().invoke(move || {
        let inner = surface
            .upgrade()
            .and_then(|surface| surface.imp().controller.borrow().clone());
        if let Some(inner) = inner {
            f(&inner);
        }
    });
}

fn spawn_worker(
    config: GlassConfig,
    requests: mpsc::Receiver<Request>,
    surface: SendWeakRef<GlassSurface>,
) {
    let result = std::thread::Builder::new()
        .name("liquid-glass".into())
        .spawn(move || {
            if let Err(err) = worker(&config, &requests, &surface) {
                warn!("glass: disabled, keeping the plain look: {err}");
                // A capture in flight must never leave the content hidden.
                on_main(&surface, |inner| inner.captured());
            }
        });
    if let Err(err) = result {
        warn!("glass: could not spawn render thread: {err}");
    }
}

/// Drains the queue: newest capture job, newest pills, or `None` on stop.
fn coalesce(
    first: Request,
    requests: &mpsc::Receiver<Request>,
) -> Option<(Option<Job>, Option<Vec<gtk::graphene::Rect>>)> {
    let mut job = None;
    let mut pills = None;
    for request in std::iter::once(first).chain(requests.try_iter()) {
        match request {
            Request::Capture(j) => job = Some(j),
            Request::Relayout(p) => pills = Some(p),
            Request::Stop => return None,
        }
    }
    Some((job, pills))
}

fn worker(
    config: &GlassConfig,
    requests: &mpsc::Receiver<Request>,
    surface: &SendWeakRef<GlassSurface>,
) -> Result<(), String> {
    let mut capture: Option<Capture> = None;
    let mut renderer: Option<GlassRenderer> = None;
    // Last capture, kept for pill-only re-renders.
    let mut last: Option<(Job, Captured)> = None;

    while let Ok(request) = requests.recv() {
        let Some((job, pills)) = coalesce(request, requests) else {
            break;
        };

        if let Some(job) = job {
            let result = match capture.as_mut() {
                Some(capture) => capture.capture(job.output.as_deref(), job.region),
                None => Capture::connect().and_then(|mut connected| {
                    let result = connected.capture(job.output.as_deref(), job.region);
                    capture = Some(connected);
                    result
                }),
            };
            on_main(surface, |inner| inner.captured());
            match result {
                Ok(captured) => last = Some((job, captured)),
                Err(err) if capture.is_none() => return Err(err),
                Err(err) => {
                    warn!("glass: capture failed: {err}");
                    continue;
                }
            }
        }

        let Some((job, captured)) = last.as_mut() else {
            continue;
        };
        if let Some(pills) = pills {
            job.pills = pills;
        }

        if renderer.is_none() {
            let created = GlassRenderer::new(config)?;
            info!("glass: rendering on {}", created.adapter());
            renderer = Some(created);
        }
        let renderer = renderer.as_mut().expect("created above");

        match render_job(renderer, job, captured) {
            Ok((rendered, stats)) => {
                let Rendered {
                    rgba,
                    width,
                    height,
                    ..
                } = rendered;
                on_main(surface, move |inner| {
                    inner.frame(rgba, width, height, stats)
                });
            }
            Err(err) => warn!("glass: render failed: {err}"),
        }
    }

    Ok(())
}

/// Physical layout of a job over a captured buffer of `buffer` pixels.
///
/// The scale is taken from the compositor's buffer rather than from GDK, so
/// fractional scales land on the same pixel grid as the capture. Returns the
/// scale, the glass rectangle `(x, y, w, h)` inside the buffer and the pills.
pub fn physical_layout(
    region: Region,
    inner: Region,
    buffer: (u32, u32),
    pills: &[gtk::graphene::Rect],
) -> (f32, (u32, u32, u32, u32), Vec<Pill>) {
    let scale = buffer.0 as f32 / region.width.max(1) as f32;
    let px = |v: i32| (v as f32 * scale).round().max(0.0) as u32;

    let x = px(inner.x).min(buffer.0.saturating_sub(1));
    let y = px(inner.y).min(buffer.1.saturating_sub(1));
    let w = px(inner.width).min(buffer.0 - x).max(1);
    let h = px(inner.height).min(buffer.1 - y).max(1);

    let pills = pills
        .iter()
        .map(|r| Pill {
            x: r.x() * scale,
            y: r.y() * scale,
            width: r.width() * scale,
            height: r.height() * scale,
        })
        .filter(|p| p.width >= 2.0 && p.height >= 2.0)
        .collect();

    (scale, (x, y, w, h), pills)
}

fn render_job(
    renderer: &mut GlassRenderer,
    job: &Job,
    captured: &Captured,
) -> Result<(Rendered, Stats), String> {
    let (scale, inner, pills) = physical_layout(
        job.region,
        job.inner,
        (captured.width, captured.height),
        &job.pills,
    );
    renderer.set_layout(inner, job.radius * scale, pills);

    let started = Instant::now();
    let rendered = renderer.render(&captured.rgba, (captured.width, captured.height), scale)?;
    debug!(
        "glass: render total {:.2} ms",
        started.elapsed().as_secs_f64() * 1000.0
    );

    let stats = Stats {
        capture_ms: captured.latency_ms,
        draw_ms: rendered.draw_ms,
        upload_ms: rendered.upload_ms,
        readback_ms: rendered.readback_ms,
    };
    Ok((rendered, stats))
}

/// Subscribes to Hyprland's event socket; false when unavailable.
fn spawn_compositor_events(surface: SendWeakRef<GlassSurface>) -> bool {
    let Ok(signature) = std::env::var("HYPRLAND_INSTANCE_SIGNATURE") else {
        return false;
    };
    let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_owned());
    let path = format!("{runtime}/hypr/{signature}/.socket2.sock");
    if std::os::unix::net::UnixStream::connect(&path).is_err() {
        return false;
    }

    std::thread::Builder::new()
        .name("liquid-glass-events".into())
        .spawn(move || {
            loop {
                let Ok(stream) = std::os::unix::net::UnixStream::connect(&path) else {
                    std::thread::sleep(Duration::from_secs(1));
                    continue;
                };
                for line in BufReader::new(stream).lines() {
                    let Ok(line) = line else { break };
                    if is_backdrop_event(line.split(">>").next().unwrap_or_default()) {
                        // Debounced on the main loop.
                        on_main(&surface, |inner| inner.schedule());
                    }
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        })
        .is_ok()
}

/// Whether a Hyprland event can change what is behind the window.
///
/// `openlayer`/`closelayer` are deliberately absent: our own popups would
/// otherwise trigger a re-capture (and a hide blink) of the bar on every open.
pub fn is_backdrop_event(event: &str) -> bool {
    matches!(
        event,
        "openwindow"
            | "closewindow"
            | "movewindow"
            | "movewindowv2"
            | "windowtitle"
            | "activewindow"
            | "workspace"
            | "workspacev2"
            | "focusedmon"
            | "createworkspace"
            | "destroyworkspace"
            | "moveworkspace"
            | "activespecial"
            | "fullscreen"
            | "changefloatingmode"
            | "monitoradded"
            | "monitorremoved"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn physical_layout_uses_the_buffer_scale() {
        // 960x62 logical padded region captured at 2x.
        let region = Region::new(0, 36, 960, 62);
        let inner = Region::new(5, 12, 950, 38);
        let pills = [gtk::graphene::Rect::new(10.0, 4.0, 60.0, 30.0)];
        let (scale, rect, pills) = physical_layout(region, inner, (1920, 124), &pills);

        assert_eq!(scale, 2.0);
        assert_eq!(
            rect,
            (10, 24, 1900, 76),
            "texture is exactly the bar in device px"
        );
        assert_eq!(
            pills[0],
            Pill {
                x: 20.0,
                y: 8.0,
                width: 120.0,
                height: 60.0
            }
        );
    }

    #[test]
    fn physical_layout_never_exceeds_the_buffer() {
        let region = Region::new(0, 0, 100, 40);
        let inner = Region::new(0, 0, 100, 40);
        let (_, rect, _) = physical_layout(region, inner, (125, 50), &[]);
        assert!(rect.0 + rect.2 <= 125 && rect.1 + rect.3 <= 50);
    }

    #[test]
    fn only_material_events_refresh() {
        assert!(is_backdrop_event("openwindow"));
        assert!(is_backdrop_event("workspacev2"));
        assert!(!is_backdrop_event("mouse"));
        assert!(!is_backdrop_event("openlayer"));
    }
}
