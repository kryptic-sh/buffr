//! [`FirefoxCdpBackend`] — stub [`Backend`] impl for the Firefox CDP browser engine.

use std::sync::Arc;

use buffr_engine::{Backend, BackendOpenOptions, BrowserEngine};

use crate::engine::FirefoxCdpEngine;

/// Firefox CDP process-model lifecycle backend.
///
/// Construct with `FirefoxCdpBackend::new()` and wrap in `Arc<dyn Backend>`.
pub struct FirefoxCdpBackend;

impl FirefoxCdpBackend {
    pub fn new() -> Self {
        FirefoxCdpBackend
    }
}

impl Default for FirefoxCdpBackend {
    fn default() -> Self {
        FirefoxCdpBackend::new()
    }
}

impl Backend for FirefoxCdpBackend {
    fn id(&self) -> &str {
        "firefox-cdp"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    /// Firefox CDP has no global init in Phase A. Always succeeds.
    fn initialize(&self, _cache_path: &str) -> Result<(), String> {
        Ok(())
    }

    fn open_engine(
        &self,
        options: BackendOpenOptions<'_>,
    ) -> Result<Arc<dyn BrowserEngine>, String> {
        let engine = FirefoxCdpEngine::new(&options).map_err(|e| e.to_string())?;
        Ok(Arc::new(engine) as Arc<dyn BrowserEngine>)
    }
}
