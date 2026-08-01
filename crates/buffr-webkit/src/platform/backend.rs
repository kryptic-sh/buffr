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
        // W1: bind the shared InternalServer up front. `WebKitEngine::new`
        // passes `None`, which leaves `buffr://` URLs untranslated — the
        // worker's very first `open_tab` fires from a GLib idle handler
        // before any post-construction setter could run, so the initial
        // tab would fail to load.
        let engine = WebKitEngine::new_with_server(&options, options.internal_server.clone())
            .map_err(|e| e.to_string())?;
        Ok(Arc::new(engine) as Arc<dyn BrowserEngine>)
    }
}
