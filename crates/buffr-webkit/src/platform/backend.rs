//! [`WebKitBackend`] — [`Backend`] impl for WPE WebKit (Linux only).

use std::sync::Arc;

use buffr_engine::{Backend, BackendOpenOptions, BrowserEngine};

use super::engine::WebKitEngine;

/// WPE WebKit process-model lifecycle backend (Linux only).
pub struct WebKitBackend;

impl WebKitBackend {
    pub fn new() -> Self {
        WebKitBackend
    }
}

impl Default for WebKitBackend {
    fn default() -> Self {
        WebKitBackend::new()
    }
}

impl Backend for WebKitBackend {
    fn id(&self) -> &str {
        "webkit"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    /// WPE WebKit has no global init in Phase 1. Always succeeds.
    fn initialize(&self, _cache_path: &str) -> Result<(), String> {
        Ok(())
    }

    fn open_engine(
        &self,
        options: BackendOpenOptions<'_>,
    ) -> Result<Arc<dyn BrowserEngine>, String> {
        tracing::debug!("webkit: WebKitBackend::open_engine (phase 1 stub)");
        let engine = WebKitEngine::new(&options).map_err(|e| e.to_string())?;
        Ok(Arc::new(engine) as Arc<dyn BrowserEngine>)
    }
}
