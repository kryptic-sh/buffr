//! `FirefoxCdpBackend` — [`buffr_engine::Backend`] implementation for headless
//! Firefox over Chrome DevTools Protocol.
//!
//! `initialize` is a no-op: Firefox spawns lazily on the first `open_engine`
//! call. The profile directory is set up by `open_engine`.

use std::path::Path;
use std::sync::Arc;

use buffr_engine::{Backend, BackendOpenOptions, BrowserEngine, NewTabHtmlProvider};

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

    /// Firefox CDP has no global init — Firefox spawns on first `open_engine`
    /// call. Always succeeds.
    fn initialize(&self, _cache_path: &str) -> Result<(), String> {
        Ok(())
    }

    fn open_engine(
        &self,
        options: BackendOpenOptions<'_>,
    ) -> Result<Arc<dyn BrowserEngine>, String> {
        let profile_dir = options
            .data_dir
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| {
                // Default: /tmp/buffr/firefox-cdp/<engine-id>
                std::path::PathBuf::from("/tmp/buffr/firefox-cdp").join(options.engine_id.as_str())
            });

        let engine = FirefoxCdpEngine::new(&profile_dir, false, None, None, None)
            .map_err(|e| e.to_string())?;
        Ok(Arc::new(engine) as Arc<dyn BrowserEngine>)
    }

    fn load_library(&self, _exe: &Path, _is_helper: bool) -> Result<(), String> {
        Ok(())
    }

    fn register_new_tab_handler(&self, _provider: NewTabHtmlProvider) {
        // Phase B: new-tab HTML provider not wired. Phase C will translate
        // buffr:// URLs to data: URLs the same way blink-cdp does.
    }

    fn register_view_source_handler(&self) {
        // Phase B: view-source: handled the same as blink-cdp via data: URL
        // translation in a future phase.
    }
}
