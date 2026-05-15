//! [`WebView2Backend`] — stub [`Backend`] impl for WebView2 (Windows only).

use std::sync::Arc;

use buffr_engine::{Backend, BackendOpenOptions, BrowserEngine};

use super::engine::WebView2Engine;

/// WebView2 process-model lifecycle backend (Windows only).
///
/// Construct with `WebView2Backend::new()` and wrap in `Arc<dyn Backend>`.
pub struct WebView2Backend;

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

    /// WebView2 has no global init in Phase A. Always succeeds.
    fn initialize(&self, _cache_path: &str) -> Result<(), String> {
        Ok(())
    }

    fn open_engine(
        &self,
        options: BackendOpenOptions<'_>,
    ) -> Result<Arc<dyn BrowserEngine>, String> {
        let engine = WebView2Engine::new(&options).map_err(|e| e.to_string())?;
        Ok(Arc::new(engine) as Arc<dyn BrowserEngine>)
    }
}
