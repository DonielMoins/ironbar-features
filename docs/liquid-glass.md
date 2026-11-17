# Liquid glass bar backdrop (`liquid-glass`)

An opt-in, Apple-style "liquid glass" surface behind the bar: the screen region
under the bar is captured, refracted and frosted on the GPU with
[`liquid-rust`](https://crates.io/crates/liquid-rust), read back, and painted
under the GTK-rendered bar content.

```toml
position = "bottom"
height = 38
margin = { top = 0, bottom = 5, left = 5, right = 5 }

glass = {
    enabled = true,
    preset = "regular",
    tint = "#171918",
    radius = 19,
    refresh = "on-change",
    adapter = "auto",
    pills = true,
}
```

Build it in:

```sh
cargo build --release --features liquid-glass
```

Without the feature the `glass` table still parses, and the bar logs a single
warning rather than failing to start.

## Why it is a cargo feature

The feature pulls in `wgpu` (and its Vulkan/GL stack) plus `liquid-rust`. That is
a large dependency tree and a second GPU device next to GSK's own context, so it
must be opt-in both at build time and per bar. The `gtk-liquid-glass` crate is an
optional dependency and only `src/glass/config.rs` is compiled when the feature
is off; the only cost on a plain build is one `Option<GlassConfig>` field on
`BarConfig`.

## Config keys

| key | values | default | notes |
|---|---|---|---|
| `enabled` | bool | `true` | Presence of the table is the opt-in; this lets a tuned config be switched off. |
| `preset` | `regular`, `clear`, `tinted` | `regular` | `regular` = frosted glass for text, `clear` = barely frosted for media. `tinted` is kept as an alias of `regular` (the tint now applies to every preset). |
| `tint` | `#rrggbb` or `#rrggbbaa` | `#1719188c` | MoinsTech ink at 55 %. Alpha is the tint strength. |
| `radius` | number (px) | `19` | Clamped to a capsule (`height / 2`); `0` derives the capsule. |
| `refresh` | `on-change`, `never`, `<ms>` | `on-change` | See below. Intervals under 100 ms are clamped. |
| `adapter` | `auto`, `integrated`, `discrete` | `auto` | Adapter choice for the wgpu device. |
| `pills` | bool | `true` | Render each module widget as its own glass pill. |

### `refresh`

- `on-change`: capture on map, then on compositor events (Hyprland's
  `.socket2.sock`, debounced 150 ms). Falls back to a 1 s interval with one log
  line when no event socket is available.
- `never`: capture once on map. Cheapest; the backdrop goes stale when windows
  move behind the bar.
- `<number>`: fixed interval in milliseconds. Each refresh costs a blocking
  compositor round trip on the capture thread.

### `adapter`

`auto` prefers the integrated GPU: it is always powered, whereas a discrete GPU
has to be woken from runtime suspend for every capture, which is both slower and
a real battery cost. `discrete` exists for benchmarking (see the numbers below).

## Ghost bar: root cause and fix

A live test showed a blurred copy of the bar drawn just beneath it. Two
independent causes, both reproduced on a headless Hyprland output
(`hyprctl output create headless`) with the owner's Waybar still running:

1. **The window was 200 px tall.** `gtk::Overlay` measures only its main child,
   and the main child is the zero-sized glass paint widget; the bar content was
   an overlay child and did not count. With no natural height GTK falls back to
   its 200 px default window size. `hyprctl layers` showed the ironbar surface
   as `950x200` with glass and `950x38` with `glass.enabled = false`. The glass
   texture was stretched over the whole 200 px surface, i.e. a smeared copy of
   the bar region under the real bar. Fix: `Overlay::set_measure_overlay`.
2. **The captured region was not where the surface was.** The region came from
   the bar's own margins, which cannot see other clients' exclusive zones: with
   Waybar reserving the top 43 px, Hyprland placed the bar at `y = 48` but the
   capture was taken at `y = 5` — of the Waybar — and that bar was refracted
   into the glass. Fix: the position is read from the compositor (`j/layers`
   on Hyprland's socket, matched by pid, namespace, output and size) and the
   anchor/margin arithmetic is only a fallback and a tie-breaker. A regression
   test replays the recorded `j/layers` reply
   (`locate::tests::regression_ghost_bar_uses_compositor_position`).

A third, latent cause is fixed as well: screencopy captures the composited
output, *including the bar itself*, so every refresh after the first would
refract the bar's own labels and pills. The compositor offers no way to
exclude one surface (Hyprland's `no_screen_share` layer rule paints a black box
instead — verified), so the widget hides its own content (`opacity 0`) for two
frames + the capture. Measured: 25 ms mean (≈ 1.5 frames at 60 Hz, n = 26) per
refresh. The first frame after mapping costs ~80 ms.

The texture is now exactly the bar in device pixels (`1900x76` for a
`950x38` bar at scale 2), cropped from a render over a padded capture.

## Look

- **Lens artefacts.** liquid-rust's presets have a 30 pt bevel with 0.7
  refraction; on a 38 pt capsule the bevel spans the whole rounded end and the
  lens sampled outside the captured strip, where clamped edge texels appear as
  black/white wedges. Now: 12 px of real backdrop is captured around the bar
  (clipped to the output), the bevel is a third of the height (≤ 14 pt),
  refraction 0.35, dispersion 0.04, no drop shadow, and pixels outside the
  rounded rectangle are cleared (anti-aliased) before upload to GTK.
- **Pills.** `glass_on_glass` is off (it made each pill refract the rendered
  capsule and its neighbours: lens wedges between pills) and pills use a
  slightly lifted ink tint, so every module reads as its own surface.
- **Tone.** The adaptive light/dark flip is off; a thin bar over a dark desktop
  was flipping to light grey and drowning white labels.

## Popups

Popups are `gtk::Popover`s, i.e. `xdg_popup` surfaces positioned relative to
the bar. Their position is the bar's compositor position + `gdk::Popup`
`position_x/y` + the popover's surface transform, so the same widget renders
them on glass. Each popup's content gets its own `GlassWidget` on first show,
captured on map (one ~25 ms hide) and on compositor events while open. The
popover's `contents` node and `.popup` background are made transparent only
after its first glass frame.

## The `gtk-liquid-glass` crate

Everything except config parsing lives in `crates/gtk-liquid-glass`, a
workspace member that other GTK4 layer-shell apps can depend on by path:

```toml
gtk-liquid-glass = { path = "../ironbar-features/crates/gtk-liquid-glass" }
```

```rust
use gtk_liquid_glass::{GlassConfig, GlassWidget, glass_overlay};

// One-liner: the glass lives as long as the overlay.
window.set_child(Some(&glass_overlay(&content, &GlassConfig::default())));

// Or keep a handle for pills / first-frame hooks / manual refresh.
let glass = GlassWidget::new(&content, &GlassConfig { radius: 24.0, ..Default::default() });
glass.set_pill_provider(|| vec![/* graphene::Rect, logical, overlay-relative */]);
glass.connect_first_frame(|child| child.add_css_class("on-glass"));
window.set_child(Some(&glass.overlay()));
glass.refresh();
```

`GlassConfig { preset, tint, radius, refresh, adapter, lens_padding,
hide_during_capture }` defaults to MoinsTech ink, radius 19, on-change refresh,
integrated GPU, 12 px lens padding, self-hide on. Lower level pieces are public
too: `capture::Capture`, `render::GlassRenderer`, `locate::*`.

The ironbar `liquid-glass` feature is now just `dep:gtk-liquid-glass`; without
it the crate (and wgpu, liquid-rust, …) is not compiled.

## How it works

```
GTK main thread                ironbar-glass thread              compositor
──────────────                 ────────────────────              ──────────
map / event / timer  ────────► region capture ──────────────────► zwlr_screencopy
                               (shm, own wl_display)  ◄────────── 1 frame
                               wgpu: upload backdrop
                               liquid-rust: glass shader
                               readback → Vec<u8>
gdk::MemoryTexture  ◄─────────
GSK snapshot()
```

- **`capture`** (`crates/gtk-liquid-glass/src/capture.rs`): `zwlr_screencopy_manager_v1` region
  capture into a `wl_shm` buffer over its own wayland connection. Region
  coordinates are *output-local logical* pixels; the compositor returns a buffer
  at the output scale (2x on HiDPI). `ext_image_copy_capture_manager_v1` would be
  the better protocol for continuously animating glass (damage frames, no
  per-request round trip), but a static bar only needs one capture per change.
- **`render`** (`crates/gtk-liquid-glass/src/render.rs`): one wgpu device, one offscreen
  `Rgba8UnormSrgb` target, one readback buffer. The scene is rebuilt per frame
  from the layout (bar capsule + module pills) and springs are settled before the
  first render — `Scene::update` must run at least once or nothing is packed.
- **`widget`** / **`locate`** (`crates/gtk-liquid-glass/src/`): the worker
  thread, the GTK widget that paints the frame, the self-hide handshake, the
  Hyprland event listener, and the window/popup geometry.
- **`style`**: the CSS background of the bar is dropped only *after* the first
  frame has been rendered, via a `.liquid-glass-active #bar` rule. A machine that
  cannot capture or render therefore keeps exactly its normal look.

The paint widget is a `gtk::Widget` subclass with a `snapshot()` override, not a
`gtk::DrawingArea`: a drawing area hands out a cairo context, and cairo cannot
blit a `gdk::Texture`. The widget measures to zero, so it never affects the bar's
layout, and it is the main child of a `gtk::Overlay` with the bar content as an
overlay child (drawn on top).

## Measured cost (this laptop)

Release build, 3840x2160 at scale 2, Hyprland, Vulkan, 60 frames per case after a
warm-up pass (`cargo run --release -p gtk-liquid-glass --example glass_bench`):

| adapter | surface | buffer | glass | upload | readback |
|---|---|---|---|---|---|
| Intel Iris Xe (integrated) | bar | 3820x76 | 2.24 ms | 0.36 ms | 0.29 ms |
| Intel Iris Xe (integrated) | 420x620 pt panel | 840x1240 | 6.30 ms | 0.90 ms | 0.33 ms |
| NVIDIA T600 (discrete) | bar | 3820x76 | 1.43 ms | 0.34 ms | 0.27 ms |
| NVIDIA T600 (discrete) | 420x620 pt panel | 840x1240 | 2.68 ms | 0.57 ms | 0.47 ms |

Capture is the expensive stage and is not in this table because the benchmark
does not touch the compositor: a bar-strip `capture_output_region` measured
~17 ms mean (p95 29.6 ms) on Hyprland, i.e. about one compositor frame, and it is
a blocking round trip. That is why the design is capture-on-change rather than
60 Hz. The first frame after startup also pays shader compilation (~58 ms glass
for the first bar frame observed in-process), which is why only steady state is
tabulated.

Readback is a full pipeline stall, so a 620 pt panel is only affordable as a
one-shot render; the bar strip is fine.

Power: the discrete GPU on this machine never leaves `active` (another
application holds a context, `runtime_suspended_time` stayed 0), so a wake cost
could not be isolated here. `adapter = "auto"` prefers the integrated GPU for
exactly that reason: a dGPU has to be woken for every capture.

```sh
cargo run --release -p gtk-liquid-glass --example glass_bench
```

## Fallbacks and failure modes

| failure | behaviour |
|---|---|
| feature not compiled in | one warning per bar, CSS background kept |
| `zwlr_screencopy_manager_v1` missing | one warning, CSS background kept |
| no usable wgpu adapter | one warning, CSS background kept |
| capture of an off-screen region | region clipped to the output; empty ⇒ skipped |
| capture or render error at runtime | one warning per failure; last frame stays on screen |

Screenshots of the result are captured with `grim` for local verification only;
desktop content is never written to the repository.

## Caveats

- GL/Vulkan coexistence: the bar holds a wgpu device next to GSK's own context.
  Both are released with the bar.
- Each refresh hides the bar for ~25 ms (see above). With `refresh = "on-change"`
  that happens on window/workspace events, when the screen is changing anyway;
  `refresh = "never"` avoids it entirely at the cost of a stale backdrop.
- Exclusive zones of other clients are only known through Hyprland's IPC. On
  other compositors the anchor/margin fallback is exact only when nothing else
  reserves space on the same edge.
- Pill rectangles are re-read once a second; a pill-only change re-renders the
  last capture without a new compositor round trip or hide.
- No damage-driven updates: the backdrop refreshes on compositor signals, so
  video playing behind the bar is refracted at the refresh cadence, not live.
