//! Opt-in Apple-style liquid glass backdrop for the bar (`liquid-glass` feature).
//!
//! * [`config`] is always compiled, so a config using it parses on a build
//!   without the cargo feature and can be rejected with one clear warning.
//! * Capture, rendering and the GTK widget live in the `gtk-liquid-glass`
//!   workspace crate, which is only a dependency when the feature is on.
//!
//! Nothing here runs unless `glass.enabled` is set in the bar config.

pub mod config;

pub use config::{GlassDecision, decide};

#[cfg(feature = "liquid-glass")]
impl config::GlassConfig {
    /// Converts the bar config into the renderer-level config.
    pub fn to_glass(&self) -> gtk_liquid_glass::GlassConfig {
        use gtk_liquid_glass as lg;

        lg::GlassConfig {
            preset: match self.preset {
                config::GlassPreset::Clear => lg::Preset::Clear,
                config::GlassPreset::Regular | config::GlassPreset::Tinted => lg::Preset::Regular,
            },
            tint: self
                .tint
                .map(|t| lg::Rgba::from_u8(t.r, t.g, t.b, f32::from(t.a) / 255.0)),
            radius: self.radius,
            refresh: match self.refresh {
                config::GlassRefresh::OnChange => lg::Refresh::OnChange,
                config::GlassRefresh::Never => lg::Refresh::Never,
                config::GlassRefresh::Interval(ms) => lg::Refresh::Interval(ms),
            },
            adapter: match self.adapter {
                config::GlassAdapter::Auto => lg::Adapter::Auto,
                config::GlassAdapter::Integrated => lg::Adapter::Integrated,
                config::GlassAdapter::Discrete => lg::Adapter::Discrete,
            },
            ..lg::GlassConfig::default()
        }
    }
}

#[cfg(all(test, feature = "liquid-glass"))]
mod tests {
    use super::config::GlassConfig;

    #[test]
    fn defaults_map_to_moinstech_ink() {
        let glass = GlassConfig::default().to_glass();
        assert_eq!(glass.radius, 19.0);
        let tint = glass.tint.expect("ink tint by default");
        let ink = gtk_liquid_glass::Rgba::INK;
        assert!((tint.r - ink.r).abs() < 1e-6 && (tint.b - ink.b).abs() < 1e-6);
        assert!((tint.a - ink.a).abs() < 0.01);
    }
}
