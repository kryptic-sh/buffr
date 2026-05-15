//! Non-Windows fallback stub for `buffr-webview2`.
//!
//! `WebView2Backend::open_engine` always returns an `Err` on non-Windows
//! hosts so `cargo check --workspace` compiles everywhere without gating
//! every call site.

use std::sync::Arc;

use buffr_engine::{Backend, BackendOpenOptions, BrowserEngine};
use thiserror::Error;

#[derive(Debug, Error)]
#[error("webview2 backend is only available on Windows")]
pub struct WebView2Error;

pub struct WebView2Backend;
pub struct WebView2Engine;

impl WebView2Backend {
    pub fn new() -> Self {
        WebView2Backend
    }
}

impl Default for WebView2Backend {
    fn default() -> Self {
        WebView2Backend::new()
    }
}

impl Backend for WebView2Backend {
    fn id(&self) -> &str {
        "webview2"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn initialize(&self, _cache_path: &str) -> Result<(), String> {
        Err("webview2 backend is only available on Windows".into())
    }

    fn open_engine(
        &self,
        _options: BackendOpenOptions<'_>,
    ) -> Result<Arc<dyn BrowserEngine>, String> {
        Err("webview2 backend is only available on Windows".into())
    }
}
