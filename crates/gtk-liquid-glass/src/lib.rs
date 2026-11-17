//! Apple-style "liquid glass" backdrop for GTK4 layer-shell windows.
//!
//! The screen region behind a layer-shell window is captured with
//! `zwlr_screencopy_manager_v1`, refracted and frosted on the GPU with
//! [`liquid_rust`], read back and painted *under* the GTK content of the window.
//!
//! ```no_run
//! # fn demo(content: &gtk::Widget, window: &gtk::Window) {
//! use gtk::prelude::*;
//! use gtk_liquid_glass::{GlassConfig, glass_overlay};
//!
//! let overlay = glass_overlay(content, &GlassConfig::default());
//! window.set_child(Some(&overlay));
//! # }
//! ```
//!
//! The window must be a `gtk4-layer-shell` window (or a toplevel whose position
//! the compositor reports; see [`locate`]). Capturing is a blocking compositor
//! round trip and GPU readback is a pipeline stall, so both run on a dedicated
//! thread per widget; the GTK main loop only builds the texture.
//!
//! **Self-capture.** A screencopy contains every surface on the output,
//! including the window the glass is drawn in. Feeding that back into the
//! glass would refract the window's own labels (the "ghost bar"). The widget
//! therefore hides its own content for the duration of each capture (see
//! [`GlassConfig::hide_during_capture`]).

pub mod capture;
pub mod locate;
pub mod render;
pub mod widget;

pub use capture::Region;
pub use render::{GlassRenderer, Pill};
pub use widget::{GlassWidget, glass_overlay};

/// Material preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Preset {
    /// Adaptive frosted glass for surfaces with text.
    #[default]
    Regular,
    /// Barely frosted glass for media backdrops.
    Clear,
}

/// GPU adapter preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Adapter {
    /// Prefer the integrated GPU (always powered; no dGPU wake per capture).
    #[default]
    Auto,
    /// Prefer the integrated GPU.
    Integrated,
    /// Prefer the discrete/offload GPU.
    Discrete,
}

/// When the backdrop is re-captured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Refresh {
    /// On map, on resize and on compositor change events (Hyprland socket2),
    /// debounced. Falls back to a 1 s interval without an event stream.
    #[default]
    OnChange,
    /// Once on map (and on resize).
    Never,
    /// Every `n` milliseconds (clamped to at least 100).
    Interval(u64),
}

/// An sRGB colour with straight alpha, `0.0..=1.0` per channel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgba {
    /// Red.
    pub r: f32,
    /// Green.
    pub g: f32,
    /// Blue.
    pub b: f32,
    /// Alpha; for a tint this is the strength.
    pub a: f32,
}

impl Rgba {
    /// MoinsTech ink, `rgba(23, 25, 24, 0.55)`.
    pub const INK: Self = Self::from_u8(23, 25, 24, 0.55);

    /// Builds a colour from 8-bit channels and a float alpha.
    pub const fn from_u8(r: u8, g: u8, b: u8, a: f32) -> Self {
        Self {
            r: r as f32 / 255.0,
            g: g as f32 / 255.0,
            b: b as f32 / 255.0,
            a,
        }
    }
}

/// Everything the glass needs to know, independent of any config format.
#[derive(Debug, Clone, PartialEq)]
pub struct GlassConfig {
    /// Material preset.
    pub preset: Preset,
    /// Tint mixed into the glass. `None` renders untinted glass.
    pub tint: Option<Rgba>,
    /// Corner radius in logical pixels; `0` makes a capsule (`height / 2`).
    pub radius: f32,
    /// Refresh policy.
    pub refresh: Refresh,
    /// GPU adapter preference.
    pub adapter: Adapter,
    /// Extra backdrop captured around the window, in logical pixels, so the
    /// lens at the edges refracts real pixels instead of clamped edge texels.
    pub lens_padding: i32,
    /// Hide the window content for the duration of a capture so the window
    /// never refracts itself. Costs a ~2 frame blink per refresh; turn off only
    /// when the compositor already excludes the surface from captures.
    pub hide_during_capture: bool,
}

impl Default for GlassConfig {
    fn default() -> Self {
        Self {
            preset: Preset::Regular,
            tint: Some(Rgba::INK),
            radius: 19.0,
            refresh: Refresh::OnChange,
            adapter: Adapter::Auto,
            lens_padding: 12,
            hide_during_capture: true,
        }
    }
}

impl GlassConfig {
    /// Corner radius in logical pixels for a surface of `height` logical pixels.
    pub fn radius_for(&self, height: f32) -> f32 {
        let capsule = height / 2.0;
        if self.radius > 0.0 {
            self.radius.min(capsule)
        } else {
            capsule
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_moinstech_ink_capsule() {
        let cfg = GlassConfig::default();
        assert_eq!(cfg.radius, 19.0);
        let tint = cfg.tint.expect("tinted by default");
        assert!((tint.r * 255.0 - 23.0).abs() < 1e-3);
        assert!((tint.g * 255.0 - 25.0).abs() < 1e-3);
        assert!((tint.b * 255.0 - 24.0).abs() < 1e-3);
        assert!((tint.a - 0.55).abs() < 1e-6);
    }

    #[test]
    fn radius_never_exceeds_a_capsule() {
        let cfg = GlassConfig::default();
        assert_eq!(cfg.radius_for(38.0), 19.0);
        assert_eq!(cfg.radius_for(20.0), 10.0);
        let capsule = GlassConfig {
            radius: 0.0,
            ..GlassConfig::default()
        };
        assert_eq!(capsule.radius_for(40.0), 20.0);
    }
}
