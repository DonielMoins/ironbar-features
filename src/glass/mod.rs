//! Opt-in Apple-style liquid glass backdrop for the bar (`liquid-glass` feature).
//!
//! The config table is always compiled, so a config using it parses on a build
//! without the cargo feature and can be rejected with one clear warning.

pub mod config;

pub use config::{GlassDecision, decide};
