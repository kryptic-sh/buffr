//! `CefBackend` — [`buffr_engine::Backend`] implementation for CEF.
//!
//! Owns all CEF process-model state: library loading, subprocess dispatch,
//! initialize/shutdown, message-pump ticking, and scheme handler registration.
//! The apps layer constructs `Arc<dyn buffr_engine::Backend>` pointing at one
//! of these; all lifecycle calls flow through the trait.

use std::path::Path;
use std::sync::Arc;

use buffr_config::DownloadsConfig;
use buffr_core::{
    DownloadNoticeQueue, EditEventSink, FindResultSink,
    hint::{HintAlphabet, HintEventSink},
    telemetry::UsageCounters,
};
use buffr_downloads::Downloads;
use buffr_history::History;
use buffr_permissions::Permissions;
use buffr_zoom::ZoomStore;

use buffr_engine::{Backend, BackendOpenOptions, BrowserEngine, NewTabHtmlProvider};

use crate::{
    BrowserHost, BuffrApp, PermissionsQueue, cef_initialize, cef_shutdown, delete_all_cookies,
    do_message_loop_work, execute_process_for_subprocess, load_cef_library,
    register_buffr_src_handler_factory, set_device_scale_factor, set_force_renderer_accessibility,
    take_scheduled_message_pump_delay_ms,
};

/// Concrete sink handles passed to `CefBackend::open_engine` via the
/// type-erased `BackendOpenOptions::sinks` field.
///
/// The apps layer constructs this struct with its concrete buffr-core
/// types and wraps it in `Box<dyn Any>`. `CefBackend::open_engine`
/// downcasts back to this type.
pub struct CefEngineSinks {
    pub history: Arc<History>,
    pub downloads: Arc<Downloads>,
    pub downloads_config: Arc<DownloadsConfig>,
    pub zoom: Arc<ZoomStore>,
    pub permissions: Arc<Permissions>,
    pub permissions_queue: PermissionsQueue,
    pub notice_queue: DownloadNoticeQueue,
    pub find_sink: FindResultSink,
    pub hint_sink: HintEventSink,
    pub edit_sink: EditEventSink,
    pub hint_alphabet: HintAlphabet,
    pub counters: Option<Arc<UsageCounters>>,
    pub show_favicons: bool,
}

/// CEF process-model lifecycle backend.
///
/// Construct with `CefBackend::new()` and wrap in `Arc<dyn Backend>`.
pub struct CefBackend;

impl CefBackend {
    pub fn new() -> Self {
        CefBackend
    }
}

impl Default for CefBackend {
    fn default() -> Self {
        CefBackend::new()
    }
}

impl Backend for CefBackend {
    fn id(&self) -> &str {
        "cef"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    /// Load the CEF framework dylib (macOS) and pin the API version.
    ///
    /// On Linux/Windows this is a no-op (CEF links dynamically through
    /// build.rs). Must be called before any other CEF entry point.
    fn load_library(&self, exe: &Path, is_helper: bool) -> Result<(), String> {
        load_cef_library(exe, is_helper)?;
        // Pin API version — must happen before execute_process or initialize.
        crate::init_cef_api();
        Ok(())
    }

    /// Dispatch subprocess (renderer/GPU/utility) and return its exit
    /// code, or -1 for the browser process.
    fn execute_subprocess(&self) -> i32 {
        execute_process_for_subprocess()
    }

    /// Initialize CEF. Internally constructs a [`BuffrApp`] instance
    /// (option A design — no `cef::App` crosses the trait boundary).
    fn initialize(&self, cache_path: &str) -> Result<(), String> {
        let mut app = BuffrApp::new();
        cef_initialize(cache_path, &mut app)
    }

    fn shutdown(&self) {
        cef_shutdown();
    }

    fn pump_message_loop(&self) {
        do_message_loop_work();
    }

    fn scheduled_pump_delay_ms(&self) -> Option<i64> {
        take_scheduled_message_pump_delay_ms()
    }

    fn delete_all_cookies(&self) {
        delete_all_cookies();
    }

    fn set_device_scale(&self, scale: f32) {
        set_device_scale_factor(scale);
    }

    fn set_force_renderer_accessibility(&self, force: bool) {
        set_force_renderer_accessibility(force);
    }

    fn register_new_tab_handler(&self, _provider: NewTabHtmlProvider) {
        // No-op: `buffr://` navigation is now handled by InternalServer HTTP
        // loopback (rewritten in `BrowserHost::cef_navigation_url`). The
        // custom CEF scheme handler has been removed. The provider closure is
        // still wired to the InternalServer `/new` route in apps/buffr-app.
    }

    fn register_view_source_handler(&self) {
        register_buffr_src_handler_factory();
    }

    fn open_engine(
        &self,
        options: BackendOpenOptions<'_>,
    ) -> Result<Arc<dyn BrowserEngine>, String> {
        if options.cache_dir.is_some() {
            ::tracing::debug!(
                engine_id = %options.engine_id,
                "cef backend ignores BackendOpenOptions.cache_dir; \
                 CEF stores persistent + ephemeral state together under data_dir/cache_path"
            );
        }

        let sinks = options
            .sinks
            .downcast::<CefEngineSinks>()
            .map_err(|_| "CefBackend::open_engine: sinks box is not CefEngineSinks".to_string())?;

        let host = BrowserHost::new_with_options(
            options.initial_url,
            sinks.history,
            sinks.downloads,
            sinks.downloads_config,
            sinks.zoom,
            sinks.permissions,
            sinks.permissions_queue,
            sinks.notice_queue,
            sinks.find_sink,
            sinks.hint_sink,
            sinks.edit_sink,
            sinks.hint_alphabet,
            options.initial_size,
            options.private,
            sinks.counters,
            sinks.show_favicons,
            options.data_dir,
            options.internal_server.clone(),
        )
        .map_err(|e| e.to_string())?;

        Ok(Arc::new(host) as Arc<dyn BrowserEngine>)
    }
}
