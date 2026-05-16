//! Non-Linux fallback stub for `buffr-webkit`.
//!
//! `WebKitBackend::open_engine` always returns an `Err` on non-Linux hosts
//! so `cargo check --workspace` compiles everywhere.

use std::sync::Arc;

use buffr_engine::{Backend, BackendOpenOptions, BrowserEngine};
use thiserror::Error;

#[derive(Debug, Error)]
#[error("wpe-webkit backend is only available on Linux")]
pub struct WebKitError;

pub struct WebKitBackend;
pub struct WebKitEngine;

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

    fn initialize(&self, _cache_path: &str) -> Result<(), String> {
        Err("wpe-webkit backend is only available on Linux".into())
    }

    fn open_engine(
        &self,
        _options: BackendOpenOptions<'_>,
    ) -> Result<Arc<dyn BrowserEngine>, String> {
        Err("wpe-webkit backend is only available on Linux".into())
    }
}
