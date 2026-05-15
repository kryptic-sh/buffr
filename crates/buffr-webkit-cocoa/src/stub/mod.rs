//! Non-macOS fallback stub for `buffr-webkit-cocoa`.
//!
//! `WebKitCocoaBackend::open_engine` always returns an `Err` on non-macOS
//! hosts so `cargo check --workspace` compiles everywhere without gating
//! every call site.

use std::sync::Arc;

use buffr_engine::{Backend, BackendOpenOptions, BrowserEngine};
use thiserror::Error;

#[derive(Debug, Error)]
#[error("webkit-cocoa backend is only available on macOS")]
pub struct WebKitCocoaError;

pub struct WebKitCocoaBackend;
pub struct WebKitCocoaEngine;

impl WebKitCocoaBackend {
    pub fn new() -> Self {
        WebKitCocoaBackend
    }
}

impl Default for WebKitCocoaBackend {
    fn default() -> Self {
        WebKitCocoaBackend::new()
    }
}

impl Backend for WebKitCocoaBackend {
    fn id(&self) -> &str {
        "webkit-cocoa"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn initialize(&self, _cache_path: &str) -> Result<(), String> {
        Err("webkit-cocoa backend is only available on macOS".into())
    }

    fn open_engine(
        &self,
        _options: BackendOpenOptions<'_>,
    ) -> Result<Arc<dyn BrowserEngine>, String> {
        Err("webkit-cocoa backend is only available on macOS".into())
    }
}
