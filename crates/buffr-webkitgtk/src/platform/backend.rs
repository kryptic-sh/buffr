//! [`WebKitGtkBackend`] — stub [`Backend`] impl for WebKitGTK (Linux only).

use std::sync::Arc;

use buffr_engine::{Backend, BackendOpenOptions, BrowserEngine};

use super::engine::WebKitGtkEngine;

/// WebKitGTK process-model lifecycle backend (Linux only).
///
/// Construct with `WebKitGtkBackend::new()` and wrap in `Arc<dyn Backend>`.
pub struct WebKitGtkBackend;

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

    /// WebKitGTK has no global init in Phase A. Always succeeds.
    fn initialize(&self, _cache_path: &str) -> Result<(), String> {
        Ok(())
    }

    fn open_engine(
        &self,
        options: BackendOpenOptions<'_>,
    ) -> Result<Arc<dyn BrowserEngine>, String> {
        let engine = WebKitGtkEngine::new(&options).map_err(|e| e.to_string())?;
        Ok(Arc::new(engine) as Arc<dyn BrowserEngine>)
    }
}
