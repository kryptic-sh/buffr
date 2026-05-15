//! [`WebKitCocoaBackend`] — stub [`Backend`] impl for WebKit Cocoa (macOS only).

use std::sync::Arc;

use buffr_engine::{Backend, BackendOpenOptions, BrowserEngine};

use super::engine::WebKitCocoaEngine;
use super::error::WebKitCocoaError;

/// WebKit Cocoa process-model lifecycle backend (macOS only).
///
/// Construct with `WebKitCocoaBackend::new()` and wrap in `Arc<dyn Backend>`.
pub struct WebKitCocoaBackend;

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

    /// WebKit Cocoa has no global init in Phase A. Always succeeds.
    fn initialize(&self, _cache_path: &str) -> Result<(), String> {
        Ok(())
    }

    fn open_engine(
        &self,
        options: BackendOpenOptions<'_>,
    ) -> Result<Arc<dyn BrowserEngine>, String> {
        let engine = WebKitCocoaEngine::new(&options).map_err(|e| e.to_string())?;
        Ok(Arc::new(engine) as Arc<dyn BrowserEngine>)
    }
}
