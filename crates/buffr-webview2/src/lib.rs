//! WebView2 (Windows) backend for buffr-engine.
//!
//! Wraps Microsoft WebView2 (Chromium-based) behind `BrowserEngine`:
//! navigation, OSR via `CapturePreview`, input forwarding (mouse +
//! `WHEEL_DELTA` scroll), hint mode, find-in-page via `ICoreWebView2Find`,
//! permissions, downloads, clipboard, and IME composition. Built on the
//! WebView2 ComPtr surface from `windows-sys`.
//!
//! The real implementation lives in `platform/` and is only compiled on Windows.
//! On all other platforms, `stub/` provides a minimal no-op that returns an
//! error from `Backend::open_engine` so `cargo check --workspace` succeeds
//! everywhere.

#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

#[cfg(target_os = "windows")]
mod platform;
#[cfg(target_os = "windows")]
pub use platform::*;

#[cfg(not(target_os = "windows"))]
mod stub;
#[cfg(not(target_os = "windows"))]
pub use stub::*;
