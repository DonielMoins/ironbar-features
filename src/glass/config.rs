//! Configuration for the opt-in `liquid-glass` bar backdrop.
//!
//! These types live outside of the `liquid-glass` feature gate on purpose: a
//! config using them must still *parse* on a build without the feature, so the
//! bar can log one clear warning and fall back to plain CSS instead of failing
//! to start.
//!
//! ```corn
//! { glass = { enabled = true, preset = "regular", tint = "#171918", radius = 19 } }
//! ```

use serde::Deserialize as Derive;
use serde::de::{self, Deserialize, Deserializer};
use std::fmt;
use std::str::FromStr;

/// The shortest refresh interval accepted for `refresh = <ms>`.
///
/// A screencopy round trip costs roughly one compositor frame (measured ~17 ms
/// on Hyprland), so anything faster than this just stalls the capture thread
/// without producing fresher content.
pub const MIN_INTERVAL_MS: u64 = 100;

/// Root `glass` table of a bar config.
#[derive(Debug, Clone, PartialEq, Derive)]
#[serde(default)]
#[cfg_attr(feature = "extras", derive(schemars::JsonSchema))]
pub struct GlassConfig {
    /// Whether the glass backdrop is rendered for this bar.
    ///
    /// Setting the table at all is the opt-in; this exists so a config can keep
    /// its tuning keys while temporarily turning the effect off.
    ///
    /// **Default**: `true`
    pub enabled: bool,

    /// Which glass material preset to use.
    ///
    /// - `regular`: adaptive frosted glass, for surfaces with text.
    /// - `clear`: barely frosted, meant for media underneath.
    /// - `tinted`: `regular` with the `tint` colour mixed in.
    ///
    /// **Default**: `regular`
    pub preset: GlassPreset,

    /// Tint colour as `#rrggbb` or `#rrggbbaa`, used by the `tinted` preset.
    /// Ignored by other presets.
    ///
    /// **Default**: `null`
    #[cfg_attr(feature = "extras", schemars(with = "String"))]
    pub tint: Option<Tint>,

    /// Corner radius of the bar capsule in pixels.
    ///
    /// `0` derives a capsule from the bar height (`height / 2`).
    ///
    /// **Default**: `0`
    pub radius: f32,

    /// How often the backdrop is re-captured.
    ///
    /// - `on-change`: on map and when the compositor reports a change
    ///   (Hyprland event socket), with a debounce. Falls back to a 1 second
    ///   interval when no compositor events are available.
    /// - `<number>`: fixed interval in milliseconds, clamped to at least
    ///   [`MIN_INTERVAL_MS`].
    /// - `never`: capture once on map and never again.
    ///
    /// **Default**: `on-change`
    #[cfg_attr(feature = "extras", schemars(with = "String"))]
    pub refresh: GlassRefresh,

    /// Which GPU adapter to render on.
    ///
    /// - `auto`: prefer the integrated GPU (low power, always awake).
    /// - `integrated`: prefer the integrated GPU, whatever `auto` resolves to.
    /// - `discrete`: prefer the discrete/offload GPU. This wakes a dGPU from
    ///   runtime suspend on every refresh, so it is slower and power-hungry on
    ///   hybrid laptops; use it only for benchmarking.
    ///
    /// **Default**: `auto`
    pub adapter: GlassAdapter,

    /// Whether to render each module widget as its own glass pill on top of the
    /// bar capsule. Rectangles are taken from the live widget allocations.
    ///
    /// **Default**: `true`
    pub pills: bool,
}

impl Default for GlassConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            preset: GlassPreset::default(),
            tint: None,
            radius: 0.0,
            refresh: GlassRefresh::default(),
            adapter: GlassAdapter::default(),
            pills: true,
        }
    }
}

impl GlassConfig {
    /// Whether the effect should actually be built.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Corner radius in physical pixels for a bar of `height` physical pixels.
    pub fn radius_px(&self, height: u32) -> f32 {
        if self.radius > 0.0 {
            self.radius
        } else {
            height as f32 / 2.0
        }
    }
}

/// The glass material preset to render with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Derive)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "extras", derive(schemars::JsonSchema))]
pub enum GlassPreset {
    /// Adaptive frosted glass.
    #[default]
    Regular,
    /// Barely frosted glass for media backdrops.
    Clear,
    /// Frosted glass with a colour mixed in.
    Tinted,
}

/// An sRGB colour with straight alpha.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tint {
    /// Red.
    pub r: u8,
    /// Green.
    pub g: u8,
    /// Blue.
    pub b: u8,
    /// Alpha, used as the tint strength.
    pub a: u8,
}

impl Tint {
    /// Builds a tint from an `0xRRGGBBAA` word.
    pub const fn from_rgba(rgb: u32, a: u8) -> Self {
        Self {
            r: ((rgb >> 16) & 0xFF) as u8,
            g: ((rgb >> 8) & 0xFF) as u8,
            b: (rgb & 0xFF) as u8,
            a,
        }
    }
}

impl FromStr for Tint {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let hex = s.trim().strip_prefix('#').unwrap_or(s.trim());

        let (rgb, alpha) = match hex.len() {
            6 => (hex, 255),
            8 => (
                &hex[..6],
                u8::from_str_radix(&hex[6..], 16)
                    .map_err(|_| format!("invalid alpha in tint '{s}'"))?,
            ),
            _ => return Err(format!("tint '{s}' must be #rrggbb or #rrggbbaa")),
        };

        let value =
            u32::from_str_radix(rgb, 16).map_err(|_| format!("invalid hex in tint '{s}'"))?;

        Ok(Self::from_rgba(value, alpha))
    }
}

impl fmt::Display for Tint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "#{:02x}{:02x}{:02x}{:02x}",
            self.r, self.g, self.b, self.a
        )
    }
}

impl<'de> Deserialize<'de> for Tint {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        raw.parse().map_err(de::Error::custom)
    }
}

/// How often the glass backdrop is refreshed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GlassRefresh {
    /// Capture on map, then on compositor change events (debounced).
    #[default]
    OnChange,
    /// Capture once on map, then never again.
    Never,
    /// Capture on a fixed interval, in milliseconds.
    Interval(u64),
}

impl FromStr for GlassRefresh {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "on-change" | "on_change" | "auto" => Ok(Self::OnChange),
            "never" | "once" => Ok(Self::Never),
            other => match other.parse::<u64>() {
                Ok(ms) => Ok(Self::Interval(ms.max(MIN_INTERVAL_MS))),
                Err(_) => Err(format!(
                    "refresh '{other}' must be 'on-change', 'never' or a millisecond count"
                )),
            },
        }
    }
}

impl<'de> Deserialize<'de> for GlassRefresh {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Derive)]
        #[serde(untagged)]
        enum Repr {
            Interval(u64),
            Named(String),
        }

        match Repr::deserialize(deserializer)? {
            Repr::Interval(ms) => Ok(Self::Interval(ms.max(MIN_INTERVAL_MS))),
            Repr::Named(name) => name.parse().map_err(de::Error::custom),
        }
    }
}

/// Which GPU adapter to render the glass on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Derive)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "extras", derive(schemars::JsonSchema))]
pub enum GlassAdapter {
    /// Prefer the integrated GPU.
    #[default]
    Auto,
    /// Require a low-power (integrated) GPU if one exists.
    Integrated,
    /// Prefer the discrete/offload GPU.
    Discrete,
}

/// What the bar should do about the `glass` config table.
///
/// Kept separate from the rendering code so that the fallback policy is a pure,
/// testable decision, and so a build without the cargo feature can still honour
/// the config by refusing it loudly instead of silently ignoring it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlassDecision {
    /// No `glass` table, or the effect is turned off in config.
    Disabled,
    /// Build the glass backdrop.
    Render,
    /// The config asks for glass but this binary was built without the
    /// `liquid-glass` feature: keep the plain CSS background and log once.
    FeatureMissing,
}

/// Decides whether a bar should render the glass backdrop.
pub fn decide(config: Option<&GlassConfig>, feature_built: bool) -> GlassDecision {
    match config {
        Some(config) if config.is_enabled() && feature_built => GlassDecision::Render,
        Some(config) if config.is_enabled() => GlassDecision::FeatureMissing,
        _ => GlassDecision::Disabled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::Value;
    use std::collections::HashMap;

    fn table(entries: Vec<(&str, Value)>) -> Value {
        Value::new(
            None,
            entries
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v))
                .collect::<HashMap<_, _>>(),
        )
    }

    fn text(v: &str) -> Value {
        Value::new(None, v)
    }

    fn parse(entries: Vec<(&str, Value)>) -> Result<GlassConfig, config::ConfigError> {
        table(entries).try_deserialize()
    }

    #[test]
    fn empty_table_uses_documented_defaults() {
        let config = parse(vec![]).expect("empty glass table should parse");

        assert!(config.enabled);
        assert_eq!(config.preset, GlassPreset::Regular);
        assert_eq!(config.tint, None);
        assert_eq!(config.radius, 0.0);
        assert_eq!(config.refresh, GlassRefresh::OnChange);
        assert_eq!(config.adapter, GlassAdapter::Auto);
        assert!(config.pills);
        assert!(config.is_enabled());
    }

    #[test]
    fn full_table_parses() {
        let config = parse(vec![
            ("enabled", Value::new(None, false)),
            ("preset", text("tinted")),
            ("tint", text("#171918")),
            ("radius", Value::new(None, 19.0f64)),
            ("refresh", text("on-change")),
            ("adapter", text("discrete")),
            ("pills", Value::new(None, false)),
        ])
        .expect("full glass table should parse");

        assert!(!config.enabled);
        assert!(!config.is_enabled());
        assert_eq!(config.preset, GlassPreset::Tinted);
        assert_eq!(config.tint, Some(Tint::from_rgba(0x17_19_18, 255)));
        assert_eq!(config.radius, 19.0);
        assert_eq!(config.refresh, GlassRefresh::OnChange);
        assert_eq!(config.adapter, GlassAdapter::Discrete);
        assert!(!config.pills);
    }

    #[test]
    fn refresh_accepts_interval_and_names() {
        let config = parse(vec![("refresh", Value::new(None, 250u64))]).expect("interval parses");
        assert_eq!(config.refresh, GlassRefresh::Interval(250));

        let config = parse(vec![("refresh", text("never"))]).expect("never parses");
        assert_eq!(config.refresh, GlassRefresh::Never);

        let config = parse(vec![("refresh", text("1000"))]).expect("string ms parses");
        assert_eq!(config.refresh, GlassRefresh::Interval(1000));
    }

    #[test]
    fn refresh_interval_is_clamped() {
        let config = parse(vec![("refresh", Value::new(None, 1u64))]).expect("1ms parses");
        assert_eq!(config.refresh, GlassRefresh::Interval(MIN_INTERVAL_MS));
    }

    #[test]
    fn invalid_refresh_is_rejected() {
        let err = parse(vec![("refresh", text("whenever"))]).expect_err("garbage refresh");
        assert!(
            format!("{err}").contains("must be 'on-change'"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn tints_parse_to_rgba() {
        assert_eq!(
            "#171918".parse::<Tint>(),
            Ok(Tint {
                r: 0x17,
                g: 0x19,
                b: 0x18,
                a: 255
            })
        );
        assert_eq!(
            "17191880".parse::<Tint>(),
            Ok(Tint::from_rgba(0x17_19_18, 0x80))
        );
        assert_eq!(
            "#FFF".parse::<Tint>(),
            Err("tint '#FFF' must be #rrggbb or #rrggbbaa".to_owned())
        );
        assert!("#zzzzzz".parse::<Tint>().is_err());
    }

    #[test]
    fn radius_falls_back_to_capsule() {
        let config = GlassConfig::default();
        assert_eq!(config.radius_px(76), 38.0);

        let config = GlassConfig {
            radius: 19.0,
            ..GlassConfig::default()
        };
        assert_eq!(config.radius_px(76), 19.0);
    }

    #[test]
    fn presets_and_adapters_parse_from_snake_case() {
        for (raw, preset) in [
            ("regular", GlassPreset::Regular),
            ("clear", GlassPreset::Clear),
            ("tinted", GlassPreset::Tinted),
        ] {
            let config = parse(vec![("preset", text(raw))]).expect("preset parses");
            assert_eq!(config.preset, preset);
        }

        for (raw, adapter) in [
            ("auto", GlassAdapter::Auto),
            ("integrated", GlassAdapter::Integrated),
            ("discrete", GlassAdapter::Discrete),
        ] {
            let config = parse(vec![("adapter", text(raw))]).expect("adapter parses");
            assert_eq!(config.adapter, adapter);
        }

        assert!(parse(vec![("preset", text("frosted"))]).is_err());
    }

    #[test]
    fn decision_respects_config_and_build_feature() {
        let enabled = GlassConfig::default();
        let disabled = GlassConfig {
            enabled: false,
            ..GlassConfig::default()
        };

        assert_eq!(decide(None, true), GlassDecision::Disabled);
        assert_eq!(decide(None, false), GlassDecision::Disabled);
        assert_eq!(decide(Some(&disabled), true), GlassDecision::Disabled);
        assert_eq!(decide(Some(&enabled), true), GlassDecision::Render);
        assert_eq!(decide(Some(&enabled), false), GlassDecision::FeatureMissing);
    }
}
