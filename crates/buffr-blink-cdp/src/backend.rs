//! `BlinkCdpBackend` — [`buffr_engine::Backend`] implementation for headless
//! Chromium over Chrome DevTools Protocol.
//!
//! Most lifecycle methods (subprocess dispatch, scheme registration, CEF
//! message pump) are no-ops here. `initialize` is also a no-op because
//! blink-cdp spawns Chromium lazily on first `open_engine` call.

use std::path::Path;
use std::sync::Arc;

use buffr_engine::{Backend, BackendOpenOptions, BrowserEngine, NewTabHtmlProvider};

use crate::BlinkCdpEngine;

/// Blink-CDP process-model lifecycle backend.
///
/// Construct with `BlinkCdpBackend::new()` and wrap in `Arc<dyn Backend>`.
pub struct BlinkCdpBackend;

impl BlinkCdpBackend {
    pub fn new() -> Self {
        BlinkCdpBackend
    }
}

impl Default for BlinkCdpBackend {
    fn default() -> Self {
        BlinkCdpBackend::new()
    }
}

impl Backend for BlinkCdpBackend {
    fn id(&self) -> &str {
        "blink-cdp"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    /// Blink-CDP has no global init — Chromium spawns on first
    /// `open_engine` call. Always succeeds.
    fn initialize(&self, _cache_path: &str) -> Result<(), String> {
        Ok(())
    }

    // load_library, execute_subprocess, shutdown, pump_message_loop,
    // scheduled_pump_delay_ms, delete_all_cookies, set_device_scale,
    // set_force_renderer_accessibility, register_new_tab_handler,
    // register_view_source_handler — all use the default no-op
    // implementations from the Backend trait.

    fn open_engine(
        &self,
        options: BackendOpenOptions<'_>,
    ) -> Result<Arc<dyn BrowserEngine>, String> {
        let data_dir = options
            .data_dir
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| {
                // Default: /tmp/buffr/blink-cdp/<engine-id>
                std::path::PathBuf::from("/tmp/buffr/blink-cdp").join(options.engine_id.as_str())
            });

        let engine = BlinkCdpEngine::new(&data_dir).map_err(|e| e.to_string())?;
        Ok(Arc::new(engine) as Arc<dyn BrowserEngine>)
    }

    // No-ops for unused library loading methods.
    fn load_library(&self, _exe: &Path, _is_helper: bool) -> Result<(), String> {
        Ok(())
    }

    fn register_new_tab_handler(&self, _provider: NewTabHtmlProvider) {
        // Blink-CDP does not serve internal buffr:// pages.
    }

    fn register_view_source_handler(&self) {
        // Blink-CDP does not serve buffr-src: pages.
    }
}
