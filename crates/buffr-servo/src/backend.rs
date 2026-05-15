//! [`ServoBackend`] — stub [`Backend`] impl for the Servo browser engine.

use std::sync::Arc;

use buffr_engine::{Backend, BackendOpenOptions, BrowserEngine};

use crate::engine::ServoEngine;

/// Servo process-model lifecycle backend.
///
/// Construct with `ServoBackend::new()` and wrap in `Arc<dyn Backend>`.
pub struct ServoBackend;

impl ServoBackend {
    pub fn new() -> Self {
        ServoBackend
    }
}

impl Default for ServoBackend {
    fn default() -> Self {
        ServoBackend::new()
    }
}

impl Backend for ServoBackend {
    fn id(&self) -> &str {
        "servo"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    /// Servo has no global init in Phase A. Always succeeds.
    fn initialize(&self, _cache_path: &str) -> Result<(), String> {
        Ok(())
    }

    fn open_engine(
        &self,
        options: BackendOpenOptions<'_>,
    ) -> Result<Arc<dyn BrowserEngine>, String> {
        let engine = ServoEngine::new(&options).map_err(|e| e.to_string())?;
        Ok(Arc::new(engine) as Arc<dyn BrowserEngine>)
    }
}
