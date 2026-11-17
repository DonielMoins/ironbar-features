//! Where on the output a window actually is.
//!
//! Wayland clients are not told their position, but screencopy needs it in
//! output-local logical coordinates. Two sources:
//!
//! 1. **Layer-shell arithmetic** ([`layer_region`]): anchors + margins + size
//!    against the monitor geometry. Exact on an otherwise empty output, but it
//!    cannot see *other* clients' exclusive zones: a top bar below another
//!    top bar (e.g. a Waybar still running) sits lower than its margins say.
//! 2. **The compositor** ([`hyprland_layer`]): Hyprland's `j/layers` IPC
//!    reports the real geometry of every layer surface, so when it is available
//!    the arithmetic is only used to pick between candidate surfaces.
//!
//! Capturing the arithmetic region when the compositor placed the surface
//! somewhere else refracts whatever *is* at that region — in the original bug,
//! the other bar, which then showed up as a ghost bar inside the glass.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use crate::capture::{Region, clip_region};

/// Layer-shell edges a surface is anchored to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Anchors {
    /// Top edge.
    pub top: bool,
    /// Bottom edge.
    pub bottom: bool,
    /// Left edge.
    pub left: bool,
    /// Right edge.
    pub right: bool,
}

/// Layer-shell margins, logical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Margins {
    /// Top margin.
    pub top: i32,
    /// Bottom margin.
    pub bottom: i32,
    /// Left margin.
    pub left: i32,
    /// Right margin.
    pub right: i32,
}

fn axis(start: bool, end: bool, m_start: i32, m_end: i32, extent: i32, size: i32) -> i32 {
    match (start, end) {
        (true, false) => m_start,
        (false, true) => extent - m_end - size,
        // Both or neither: centred in the space left by the margins.
        (true, true) => m_start + (extent - m_start - m_end - size) / 2,
        (false, false) => (extent - size) / 2,
    }
}

/// Output-local region of a layer surface of `size` on an output of
/// `output_size`, from its anchors and margins alone (no exclusive zones).
pub fn layer_region(
    output_size: (i32, i32),
    size: (i32, i32),
    anchors: Anchors,
    margins: Margins,
) -> Option<Region> {
    if size.0 <= 0 || size.1 <= 0 {
        return None;
    }

    let x = axis(
        anchors.left,
        anchors.right,
        margins.left,
        margins.right,
        output_size.0,
        size.0,
    );
    let y = axis(
        anchors.top,
        anchors.bottom,
        margins.top,
        margins.bottom,
        output_size.1,
        size.1,
    );

    Some(Region::new(x, y, size.0, size.1))
}

/// Finds this process' layer surface in a Hyprland `j/layers` reply.
///
/// Candidates must be on `output`, belong to `pid`, carry `namespace` and have
/// exactly `size`. Several surfaces can match (a bar and its popups share a
/// namespace), so the one closest to `guess` wins. The result is output-local:
/// Hyprland reports global coordinates, `output_origin` is subtracted.
pub fn hyprland_layer(
    json: &str,
    output: &str,
    pid: u32,
    namespace: &str,
    size: (i32, i32),
    output_origin: (i32, i32),
    guess: Option<Region>,
) -> Option<Region> {
    let root: serde_json::Value = serde_json::from_str(json).ok()?;
    let levels = root.get(output)?.get("levels")?.as_object()?;

    let int = |v: &serde_json::Value, k: &str| v.get(k).and_then(serde_json::Value::as_i64);

    levels
        .values()
        .filter_map(serde_json::Value::as_array)
        .flatten()
        .filter(|layer| {
            int(layer, "pid") == Some(i64::from(pid))
                && layer.get("namespace").and_then(serde_json::Value::as_str) == Some(namespace)
                && int(layer, "w") == Some(i64::from(size.0))
                && int(layer, "h") == Some(i64::from(size.1))
        })
        .filter_map(|layer| {
            Some(Region::new(
                int(layer, "x")? as i32 - output_origin.0,
                int(layer, "y")? as i32 - output_origin.1,
                size.0,
                size.1,
            ))
        })
        .min_by_key(|region| {
            guess.map_or(0, |g| {
                (i64::from(region.x - g.x)).abs() + (i64::from(region.y - g.y)).abs()
            })
        })
}

/// Sends one request to Hyprland's command socket, if running under Hyprland.
pub fn hyprland_request(request: &str) -> Option<String> {
    let signature = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok()?;
    let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_owned());
    let mut stream =
        UnixStream::connect(format!("{runtime}/hypr/{signature}/.socket.sock")).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .ok()?;
    stream.write_all(request.as_bytes()).ok()?;

    let mut reply = String::new();
    stream.read_to_string(&mut reply).ok()?;
    Some(reply)
}

/// Grows `region` by `pad` on every side, clipped to `bounds`.
///
/// Returns the padded region and the offset of `region` inside it, so a render
/// over the padded backdrop can be cropped back to exactly the window.
pub fn pad_region(region: Region, pad: i32, bounds: (i32, i32)) -> Option<(Region, (i32, i32))> {
    let inner = clip_region(region, bounds)?;
    let padded = clip_region(
        Region::new(
            inner.x - pad,
            inner.y - pad,
            inner.width + 2 * pad,
            inner.height + 2 * pad,
        ),
        bounds,
    )?;
    Some((padded, (inner.x - padded.x, inner.y - padded.y)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUT: (i32, i32) = (960, 540);
    const TOP_BAR: Anchors = Anchors {
        top: true,
        bottom: false,
        left: true,
        right: true,
    };

    fn margins(top: i32, bottom: i32, left: i32, right: i32) -> Margins {
        Margins {
            top,
            bottom,
            left,
            right,
        }
    }

    /// Recorded from `hyprctl layers -j` on a headless output where a Waybar
    /// (pid 2718) already reserved the top 43 px: the ironbar surface was
    /// placed at y = 48, not at its margin of 5. Capturing y = 5 refracted the
    /// Waybar — the ghost bar.
    const LAYERS: &str = r#"{"glass-wt": {"levels": {
        "0": [{"x": 1920, "y": 0, "w": 960, "h": 540, "namespace": "wallpaper", "pid": 2775}],
        "1": [],
        "2": [{"x": 1925, "y": 5, "w": 950, "h": 38, "namespace": "waybar", "pid": 2718},
              {"x": 1925, "y": 48, "w": 950, "h": 38, "namespace": "ironbar", "pid": 4242},
              {"x": 2400, "y": 91, "w": 300, "h": 200, "namespace": "ironbar", "pid": 4242}],
        "3": []}}}"#;

    #[test]
    fn anchored_top_bar_math() {
        let r = layer_region(OUT, (950, 38), TOP_BAR, margins(5, 0, 5, 5)).unwrap();
        assert_eq!(r, Region::new(5, 5, 950, 38));
    }

    #[test]
    fn bottom_right_and_centred_surfaces() {
        let br = Anchors {
            bottom: true,
            right: true,
            ..Anchors::default()
        };
        let r = layer_region(OUT, (300, 200), br, margins(0, 10, 0, 20)).unwrap();
        assert_eq!(r, Region::new(960 - 20 - 300, 540 - 10 - 200, 300, 200));

        let r = layer_region(OUT, (100, 40), Anchors::default(), Margins::default()).unwrap();
        assert_eq!(r, Region::new(430, 250, 100, 40));
    }

    #[test]
    fn regression_ghost_bar_uses_compositor_position() {
        let guess = layer_region(OUT, (950, 38), TOP_BAR, margins(5, 0, 5, 5));
        let real = hyprland_layer(
            LAYERS,
            "glass-wt",
            4242,
            "ironbar",
            (950, 38),
            (1920, 0),
            guess,
        )
        .expect("own layer found");

        assert_eq!(real, Region::new(5, 48, 950, 38));
        assert_ne!(
            Some(real),
            guess,
            "margin math alone captures the other bar"
        );
    }

    #[test]
    fn other_clients_and_sizes_never_match() {
        // Waybar has the same size and namespace-free geometry; wrong pid.
        assert!(
            hyprland_layer(
                LAYERS,
                "glass-wt",
                2718,
                "ironbar",
                (950, 38),
                (1920, 0),
                None
            )
            .is_none()
        );
        assert!(
            hyprland_layer(
                LAYERS,
                "glass-wt",
                4242,
                "ironbar",
                (951, 38),
                (1920, 0),
                None
            )
            .is_none()
        );
        assert!(
            hyprland_layer(LAYERS, "eDP-1", 4242, "ironbar", (950, 38), (0, 0), None).is_none()
        );
        assert!(
            hyprland_layer(
                "not json",
                "glass-wt",
                4242,
                "ironbar",
                (950, 38),
                (0, 0),
                None
            )
            .is_none()
        );
    }

    #[test]
    fn popup_surface_is_found_by_its_size() {
        let r = hyprland_layer(
            LAYERS,
            "glass-wt",
            4242,
            "ironbar",
            (300, 200),
            (1920, 0),
            None,
        );
        assert_eq!(r, Some(Region::new(480, 91, 300, 200)));
    }

    #[test]
    fn padding_is_clipped_and_offset_reported() {
        let (padded, offset) = pad_region(Region::new(5, 48, 950, 38), 12, OUT).unwrap();
        assert_eq!(padded, Region::new(0, 36, 960, 62));
        assert_eq!(offset, (5, 12));

        let (padded, offset) = pad_region(Region::new(100, 100, 50, 50), 12, OUT).unwrap();
        assert_eq!(padded, Region::new(88, 88, 74, 74));
        assert_eq!(offset, (12, 12));
    }
}
