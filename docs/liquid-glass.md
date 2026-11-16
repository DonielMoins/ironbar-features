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
must be opt-in both at build time and per bar. Everything in `src/glass/` other
than `config.rs` is compiled out when the feature is off; the only cost on a
plain build is one `Option<GlassConfig>` field on `BarConfig`.

## Config keys

| key | values | default | notes |
|---|---|---|---|
| `enabled` | bool | `true` | Presence of the table is the opt-in; this lets a tuned config be switched off. |
| `preset` | `regular`, `clear`, `tinted` | `regular` | `regular` = adaptive frosted glass for text, `clear` = barely frosted for media, `tinted` = `regular` plus the tint colour. |
| `tint` | `#rrggbb` or `#rrggbbaa` | none | Only used by the `tinted` preset. Alpha is the tint strength. |
| `radius` | number (px) | `0` | `0` derives a capsule from the bar height (`height / 2`). |
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

- **`capture`** (`src/glass/capture.rs`): `zwlr_screencopy_manager_v1` region
  capture into a `wl_shm` buffer over its own wayland connection. Region
  coordinates are *output-local logical* pixels; the compositor returns a buffer
  at the output scale (2x on HiDPI). `ext_image_copy_capture_manager_v1` would be
  the better protocol for continuously animating glass (damage frames, no
  per-request round trip), but a static bar only needs one capture per change.
- **`render`** (`src/glass/render.rs`): one wgpu device, one offscreen
  `Rgba8UnormSrgb` target, one readback buffer. The scene is rebuilt per frame
  from the layout (bar capsule + module pills) and springs are settled before the
  first render — `Scene::update` must run at least once or nothing is packed.
- **`backdrop`** (`src/glass/backdrop.rs`): the worker thread, the GTK widget
  that paints the frame, the Hyprland event listener, and the region geometry.
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
warm-up pass (`cargo run --release --features liquid-glass --example glass_bench`):

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
cargo run --release --features liquid-glass --example glass_bench
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
- Popups are not glass (yet). Each popover is its own surface, so it needs its
  own capture region and its own glass widget; that is a follow-up.
- Nested pills are rendered with `Material::clear()` over the bar capsule. Glass
  over glass is nearly invisible when the materials match, so module *labels* are
  what stays readable; a tint is needed if pills must read as separate surfaces.
- No damage-driven updates: the backdrop refreshes on compositor signals, so
  video playing behind the bar is refracted at the refresh cadence, not live.
