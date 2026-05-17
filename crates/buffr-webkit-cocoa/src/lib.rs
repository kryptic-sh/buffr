//! WKWebView (macOS) backend for buffr-engine.
//!
//! Wraps `WKWebView` behind `BrowserEngine`: navigation, OSR via
//! `CapturePreview`, input forwarding, hint mode, find-in-page,
//! permissions, downloads, clipboard, favicon, and IME composition.
//! Built on `objc2` with `Retained<NSObject>` subclasses for the
//! navigation/UI/script-message/download delegates.
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
