//! [`BlitzBackend`] — [`Backend`] impl for the Blitz browser engine.

use std::sync::Arc;

use buffr_engine::{Backend, BackendOpenOptions, BrowserEngine};

use crate::engine::BlitzEngine;

/// Blitz process-model lifecycle backend.
///
/// Construct with `BlitzBackend::new()` and wrap in `Arc<dyn Backend>`.
pub struct BlitzBackend;

impl BlitzBackend {
    pub fn new() -> Self {
        BlitzBackend
    }
}

impl Default for BlitzBackend {
    fn default() -> Self {
        BlitzBackend::new()
    }
}

impl Backend for BlitzBackend {
    fn id(&self) -> &str {
        "blitz"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    /// Blitz has no global init — always succeeds.
    fn initialize(&self, _cache_path: &str) -> Result<(), String> {
        tracing::info!("blitz backend: initialize (no-op)");
        Ok(())
    }

    fn open_engine(
        &self,
        options: BackendOpenOptions<'_>,
    ) -> Result<Arc<dyn BrowserEngine>, String> {
        let engine = BlitzEngine::new(&options).map_err(|e| e.to_string())?;
        Ok(Arc::new(engine) as Arc<dyn BrowserEngine>)
    }
}
