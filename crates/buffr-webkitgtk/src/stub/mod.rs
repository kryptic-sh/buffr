//! Non-Linux fallback stub for `buffr-webkitgtk`.
//!
//! `WebKitGtkBackend::open_engine` always returns an `Err` on non-Linux
//! hosts so `cargo check --workspace` compiles everywhere without gating
//! every call site.

use std::sync::Arc;

use buffr_engine::{Backend, BackendOpenOptions, BrowserEngine};
use thiserror::Error;

#[derive(Debug, Error)]
#[error("webkitgtk backend is only available on Linux")]
pub struct WebKitGtkError;

pub struct WebKitGtkBackend;
pub struct WebKitGtkEngine;

impl WebKitGtkBackend {
    pub fn new() -> Self {
        WebKitGtkBackend
    }
}

impl Default for WebKitGtkBackend {
    fn default() -> Self {
        WebKitGtkBackend::new()
    }
}

impl Backend for WebKitGtkBackend {
    fn id(&self) -> &str {
        "webkitgtk"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn initialize(&self, _cache_path: &str) -> Result<(), String> {
        Err("webkitgtk backend is only available on Linux".into())
    }

    fn open_engine(
        &self,
        _options: BackendOpenOptions<'_>,
    ) -> Result<Arc<dyn BrowserEngine>, String> {
        Err("webkitgtk backend is only available on Linux".into())
    }
}
