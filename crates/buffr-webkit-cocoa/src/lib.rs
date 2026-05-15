//! WebKit Cocoa (macOS) backend for buffr-engine.
//!
//! Phase A stub: all engine methods return `EngineError::Unimplemented`.
//! Phase B will wire real WebKit/WKWebView integration.
//!
//! The real implementation lives in `platform/` and is only compiled on macOS.
//! On all other platforms, `stub/` provides a minimal no-op that returns an
//! error from `Backend::open_engine` so `cargo check --workspace` succeeds
//! everywhere.

#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

#[cfg(target_os = "macos")]
mod platform;
#[cfg(target_os = "macos")]
pub use platform::*;

#[cfg(not(target_os = "macos"))]
mod stub;
#[cfg(not(target_os = "macos"))]
pub use stub::*;
