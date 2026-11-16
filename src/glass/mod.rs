//! Opt-in Apple-style liquid glass backdrop for the bar (`liquid-glass` feature).
//!
//! Shape of the feature:
//!
//! * [`config`] is always compiled, so a config using it parses on a build
//!   without the cargo feature and can be rejected with one clear warning.
//! * [`capture`] grabs the screen region under the bar with
//!   `zwlr_screencopy_manager_v1` on its own wayland connection.
//! * [`render`] runs the `liquid-rust` glass shader over that backdrop on a wgpu
//!   device, offscreen, and reads the result back.
//! * [`backdrop`] owns the worker thread and the `gtk::DrawingArea` that paints
//!   the result under the bar content.
//!
//! Nothing here runs unless `glass.enabled` is set in the bar config, and the
//! whole module tree is empty without the cargo feature.

pub mod config;

pub use config::{GlassDecision, decide};

#[cfg(feature = "liquid-glass")]
pub mod backdrop;
#[cfg(feature = "liquid-glass")]
pub mod capture;
#[cfg(feature = "liquid-glass")]
pub mod render;

#[cfg(feature = "liquid-glass")]
pub use backdrop::GlassBackdrop;
