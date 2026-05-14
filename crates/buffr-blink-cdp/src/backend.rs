//! `BlinkCdpBackend` — [`buffr_engine::Backend`] implementation for headless
//! Chromium over Chrome DevTools Protocol.
//!
//! Most lifecycle methods (subprocess dispatch, scheme registration, CEF
//! message pump) are no-ops here. `initialize` is also a no-op because
//! blink-cdp spawns Chromium lazily on first `open_engine` call.

use std::path::Path;
use std::sync::{Arc, Mutex};

use buffr_core::DownloadNoticeQueue;
use buffr_downloads::Downloads;
use buffr_engine::{Backend, BackendOpenOptions, BrowserEngine, NewTabHtmlProvider};

use crate::BlinkCdpEngine;
use crate::engine::HtmlProvider;

/// Blink-CDP process-model lifecycle backend.
///
/// Construct with `BlinkCdpBackend::new()` and wrap in `Arc<dyn Backend>`.
///
/// Stores `buffr://` HTML providers registered via
/// [`Backend::register_new_tab_handler`] and wires them into each engine
/// created by [`Backend::open_engine`].
pub struct BlinkCdpBackend {
    /// Stored new-tab HTML provider (set by `register_new_tab_handler`).
    newtab_provider: Mutex<Option<HtmlProvider>>,
}

impl BlinkCdpBackend {
    pub fn new() -> Self {
        BlinkCdpBackend {
            newtab_provider: Mutex::new(None),
        }
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

        // Downcast the type-erased download store and notice queue, if provided.
        let downloads: Option<Arc<Downloads>> = options
            .downloads
            .as_ref()
            .and_then(|any| any.downcast_ref::<Arc<Downloads>>())
            .cloned();
        let notice_queue: Option<DownloadNoticeQueue> = options
            .notice_queue
            .as_ref()
            .and_then(|any| any.downcast_ref::<DownloadNoticeQueue>())
            .cloned();
        let download_dir = options.download_dir.map(|p| p.to_path_buf());

        let engine = BlinkCdpEngine::new(
            &data_dir,
            download_dir.as_deref(),
            downloads,
            notice_queue,
            None,
        )
        .map_err(|e| e.to_string())?;

        // Wire up any registered HTML providers (Phase 8f, #81).
        if let Ok(guard) = self.newtab_provider.lock()
            && let Some(ref provider) = *guard
        {
            engine.set_newtab_html_provider(Arc::clone(provider));
        }

        Ok(Arc::new(engine) as Arc<dyn BrowserEngine>)
    }

    // No-ops for unused library loading methods.
    fn load_library(&self, _exe: &Path, _is_helper: bool) -> Result<(), String> {
        Ok(())
    }

    /// Store the new-tab HTML provider for use in subsequent `open_engine` calls.
    ///
    /// The blink-cdp backend translates `buffr://new` navigations to
    /// `data:text/html;base64,...` URLs at the engine layer (Phase 8f, #81).
    /// The provider is invoked on each navigation to produce fresh HTML.
    fn register_new_tab_handler(&self, provider: NewTabHtmlProvider) {
        if let Ok(mut guard) = self.newtab_provider.lock() {
            *guard = Some(provider);
        }
    }

    fn register_view_source_handler(&self) {
        // view-source: is handled at the engine layer (Phase 8f, #81);
        // no global registration needed.
    }
}
