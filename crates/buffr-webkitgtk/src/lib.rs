//! WebKitGTK (Linux) backend for buffr-engine.
//!
//! Phase A stub: all engine methods return `EngineError::Unimplemented`.
//! Phase B will wire real WebKitGTK integration.
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
