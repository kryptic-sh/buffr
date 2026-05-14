//! `Backend` trait — process-model lifecycle abstraction.
//!
//! One implementation per concrete browser engine crate (buffr-cef,
//! buffr-blink-cdp). The apps layer programs against this trait so
//! `apps/buffr-app` imports zero concrete engine symbols beyond a single
//! `Arc<dyn Backend>` construction site per backend type.

use std::path::Path;
use std::sync::Arc;

use crate::BrowserEngine;
use crate::engine_id::EngineId;

/// Closure-based new-tab HTML provider. Invoked on each `buffr://new`
/// request to produce fresh page bytes (dynamic keybinds, splash art).
///
/// Defined here so apps-layer callers can name the type without
/// importing `buffr-cef` directly.
pub type NewTabHtmlProvider = Arc<dyn Fn() -> Vec<u8> + Send + Sync>;

/// Engine-agnostic options for [`Backend::open_engine`].
///
/// Fields that are fully backend-specific (e.g. sink handles from
/// `buffr-core`) are erased as [`std::any::Any`] so `buffr-engine`
/// stays free of `buffr-core` imports. Each backend's `open_engine`
/// implementation downcasts `sinks` back to its concrete type.
pub struct BackendOpenOptions<'a> {
    /// Logical identifier for this engine instance (from config).
    pub engine_id: EngineId,
    /// Optional on-disk data / profile directory.
    pub data_dir: Option<&'a Path>,
    /// URL to navigate to when the first tab is created.
    pub initial_url: &'a str,
    /// Requested OSR frame rate in Hz.
    pub frame_rate: i32,
    /// Device pixel ratio (e.g. 1.0, 1.5, 2.0).
    pub device_scale: f64,
    /// Initial viewport dimensions in physical pixels `(width, height)`.
    pub initial_size: (u32, u32),
    /// Whether this engine runs in private (incognito) mode.
    pub private: bool,
    /// Directory where downloads should be saved.
    ///
    /// Used by the blink-cdp backend to configure
    /// `Browser.setDownloadBehavior`. CEF backends manage their own
    /// download directory via the sinks' `DownloadsConfig`.
    pub download_dir: Option<&'a Path>,
    /// Shared downloads store. blink-cdp writes to this directly when
    /// downloads start / progress / complete. CEF ignores this field
    /// (continues using its existing `CefEngineSinks` wiring).
    pub downloads: Option<Arc<dyn std::any::Any + Send + Sync>>,
    /// Shared download-notice queue. blink-cdp pushes notices here on
    /// `Started` / `Completed` / `Canceled`. CEF ignores this field.
    pub notice_queue: Option<Arc<dyn std::any::Any + Send + Sync>>,
    /// Backend-specific sink handles, type-erased.
    ///
    /// CEF: expects `Box<CefEngineSinks>` (defined in buffr-cef).
    /// Blink-CDP: ignored (pass `Box::new(())`).
    pub sinks: Box<dyn std::any::Any + Send>,
}

/// Process-model lifecycle hooks. One impl per backend crate.
///
/// `Backend` covers the *process-model* surface: library loading,
/// subprocess dispatch, initialize/shutdown, message-pump tick, and
/// scheme registration. Engine construction goes through
/// [`Backend::open_engine`].
///
/// All methods have safe no-op defaults so backends only override what
/// they need.
pub trait Backend: Send + Sync {
    /// Backend identifier. Matches `EngineInstance::backend` in config.
    fn id(&self) -> &str;

    /// Return `self` as `&dyn std::any::Any` to allow downcasting to
    /// the concrete type when needed.
    fn as_any(&self) -> &dyn std::any::Any;

    /// Load any shared libraries the backend requires before process
    /// start. No-op on Linux/Windows; CEF loads the framework dylib on
    /// macOS.
    fn load_library(&self, _exe: &Path, _is_helper: bool) -> Result<(), String> {
        Ok(())
    }

    /// Run the subprocess event loop if this process is a backend helper
    /// (renderer, GPU, utility). Returns the exit code (≥ 0) for child
    /// processes or -1 if this is the browser process.
    ///
    /// CEF: calls `cef::execute_process`. Blink-CDP has no subprocess
    /// model; the default -1 is always correct.
    fn execute_subprocess(&self) -> i32 {
        -1
    }

    /// Initialize the backend's global state. Called once in the browser
    /// process after `load_library` and the subprocess check.
    ///
    /// `cache_path` is the root cache/profile directory. CEF backends
    /// construct their `App` object internally here (option A design —
    /// no CEF-specific types cross the boundary).
    fn initialize(&self, cache_path: &str) -> Result<(), String>;

    /// Shut the backend down cleanly. Called once before process exit.
    fn shutdown(&self) {}

    /// Pump one tick of the backend's message loop. Called on every
    /// winit `about_to_wait`. CEF requires this on all platforms; other
    /// backends may no-op when they drive their own worker thread.
    fn pump_message_loop(&self) {}

    /// Return the next scheduled pump delay from an external message
    /// pump (macOS / CEF only). Returns `None` when no delay is queued.
    fn scheduled_pump_delay_ms(&self) -> Option<i64> {
        None
    }

    /// Delete all browser cookies. Called before shutdown when
    /// `--clear-cookies-on-exit` is active.
    fn delete_all_cookies(&self) {}

    /// Forward the device-pixel-ratio hint to the backend before
    /// `initialize`. CEF passes this as `--force-device-scale-factor`.
    fn set_device_scale(&self, _scale: f32) {}

    /// Enable or disable the renderer accessibility tree. CEF maps this
    /// to `--force-renderer-accessibility`. Must be called before
    /// `initialize`.
    fn set_force_renderer_accessibility(&self, _force: bool) {}

    /// Register the `buffr://new` (new-tab) custom scheme handler.
    ///
    /// `provider` is invoked on each request to produce fresh HTML.
    /// Called after `initialize`, before the first engine opens. Backends
    /// that don't serve internal pages may no-op this.
    fn register_new_tab_handler(&self, _provider: NewTabHtmlProvider) {}

    /// Register the `buffr-src:` (view-source) custom scheme handler.
    ///
    /// Called after `initialize`, before the first engine opens. Backends
    /// that don't serve internal pages may no-op this.
    fn register_view_source_handler(&self) {}

    /// Construct a new [`BrowserEngine`] instance.
    ///
    /// `options.sinks` carries backend-specific data type-erased as
    /// `Box<dyn Any>`. Each backend's implementation downcasts to its
    /// concrete sink struct.
    fn open_engine(
        &self,
        options: BackendOpenOptions<'_>,
    ) -> Result<Arc<dyn BrowserEngine>, String>;
}
