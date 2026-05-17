//! WPE WebKit (Linux native) backend for buffr-engine.
//!
//! Phase 2 shipped (OSR readback) and Phase 3 v1 shipped (Wayland subsurface
//! compositing via a custom `BuffrDisplayWayland` GObject subclass that
//! reuses the host's `wl_display`). Navigation, input, hint mode, find,
//! permissions, downloads, IME, clipboard, and DMA-BUF → `wl_buffer`
//! bridging are wired through `platform/`. See umbrella issue #109 for
//! the phase plan; Phase 3 polish items (#147–#150) and hands-on
//! verification on Wayland (#127) remain.
//!
//! The real implementation lives in `platform/` and is only compiled on Linux.
//! On all other platforms, `stub/` provides a minimal no-op that returns an
//! error from `Backend::open_engine` so `cargo check --workspace` succeeds
//! everywhere.

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

#[cfg(target_os = "linux")]
mod platform;
#[cfg(target_os = "linux")]
pub use platform::*;

#[cfg(not(target_os = "linux"))]
mod stub;
#[cfg(not(target_os = "linux"))]
pub use stub::*;
