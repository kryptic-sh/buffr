//! [`LadybirdBackend`] — stub [`Backend`] impl for the Ladybird browser engine.

use std::sync::Arc;

use buffr_engine::{Backend, BackendOpenOptions, BrowserEngine};

use crate::engine::LadybirdEngine;

/// Ladybird process-model lifecycle backend.
///
/// Construct with `LadybirdBackend::new()` and wrap in `Arc<dyn Backend>`.
pub struct LadybirdBackend;

impl LadybirdBackend {
    pub fn new() -> Self {
        LadybirdBackend
    }
}

impl Default for LadybirdBackend {
    fn default() -> Self {
        LadybirdBackend::new()
    }
}

impl Backend for LadybirdBackend {
    fn id(&self) -> &str {
        "ladybird"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    /// Ladybird has no global init in Phase A. Always succeeds.
    fn initialize(&self, _cache_path: &str) -> Result<(), String> {
        Ok(())
    }

    fn open_engine(
        &self,
        options: BackendOpenOptions<'_>,
    ) -> Result<Arc<dyn BrowserEngine>, String> {
        let engine = LadybirdEngine::new(&options).map_err(|e| e.to_string())?;
        Ok(Arc::new(engine) as Arc<dyn BrowserEngine>)
    }
}
