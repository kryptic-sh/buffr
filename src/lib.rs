//! CEF integration and browser host for buffr.
//!
//! All CEF-specific code lives here. `buffr-core` depends on
//! `buffr-engine` (the agnostic trait) only; this crate provides the
//! concrete `BrowserHost` implementation backed by Chromium Embedded
//! Framework.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

pub mod app;
pub mod audio;
pub(crate) mod convert;
pub mod handlers;
pub mod host;
pub mod new_tab;
pub mod osr;
pub mod permissions;
pub mod view_source_scheme;

pub use app::{
    BuffrApp, ProfilePaths, device_scale_factor, force_renderer_accessibility_enabled,
    profile_paths, set_device_scale_factor, set_force_renderer_accessibility,
    take_scheduled_message_pump_delay_ms,
};
pub use audio::{
    AudioEvent, AudioEventQueue, AudioState, AudioStateSink, any_audio_active, drain_audio_events,
    new_audio_event_queue, new_audio_state_sink,
};
pub use host::{BrowserHost, ClipboardReader, HintStatus, Tab, TabSession};
// Re-export unified types from buffr-engine so callers using `buffr_cef::TabId`
// keep working without also importing buffr-engine directly.
pub use buffr_engine::{BrowserEngine, TabId, TabSummary};
pub use new_tab::{
    NEW_TAB_HTML_TEMPLATE, NEW_TAB_KEYBINDS_MARKER, NEW_TAB_SPLASH_ART_MARKER, NEW_TAB_URL,
    NewTabHtmlProvider, SETTINGS_URL, SettingsHtmlProvider, register_buffr_handler_factory,
    register_buffr_handler_factory_static, register_buffr_handler_factory_with_settings,
    register_buffr_scheme, settings_html,
};
pub use osr::{OsrFrame, OsrViewState, PopupFrameMap, SharedOsrFrame, SharedOsrViewState};
pub use permissions::{
    PendingPermission, PermissionsQueue, PromptOutcome, capabilities_for_media_mask,
    capabilities_for_request_mask, drain_with_defer as drain_permissions_with_defer,
    new_queue as new_permissions_queue, peek_front as peek_permission_front,
    pop_front as pop_permission_front, precheck as precheck_permission,
    queue_len as permissions_queue_len,
};
pub use view_source_scheme::{register_buffr_src_handler_factory, register_buffr_src_scheme};

/// URLs queued by `LifeSpanHandler::on_before_popup` for dispositions
/// that should open as a new tab (`NEW_FOREGROUND_TAB`,
/// `NEW_BACKGROUND_TAB`). `NEW_POPUP` / `NEW_WINDOW` are not enqueued.
pub type PopupQueue = Arc<Mutex<VecDeque<String>>>;

pub fn new_popup_queue() -> PopupQueue {
    Arc::new(Mutex::new(VecDeque::new()))
}

pub fn drain_popup_urls(q: &PopupQueue) -> Vec<String> {
    if let Ok(mut g) = q.lock() {
        return g.drain(..).collect();
    }
    Vec::new()
}

/// A popup browser window ready to render. Emitted by the lifespan
/// handler on `on_after_created` for popup browsers.
pub struct PopupCreated {
    /// CEF `Browser::identifier()` for the new popup browser.
    pub browser_id: i32,
    /// Initial URL from `on_before_popup`. May be empty.
    pub url: String,
    /// OSR frame buffer shared with the paint handler.
    pub frame: SharedOsrFrame,
    /// OSR viewport state.
    pub view: SharedOsrViewState,
}

/// Queue of popup-created events.
pub type PopupCreateSink = Arc<Mutex<VecDeque<PopupCreated>>>;

/// Queue of `browser_id` values for closed popup browsers.
pub type PopupCloseSink = Arc<Mutex<VecDeque<i32>>>;

pub fn new_popup_create_sink() -> PopupCreateSink {
    Arc::new(Mutex::new(VecDeque::new()))
}

pub fn new_popup_close_sink() -> PopupCloseSink {
    Arc::new(Mutex::new(VecDeque::new()))
}

/// Single-slot pending popup alloc: allocated by `on_before_popup`,
/// consumed by `on_after_created`.
pub type PendingPopupAlloc = Arc<Mutex<Option<(SharedOsrFrame, SharedOsrViewState, String)>>>;

pub fn new_pending_popup_alloc() -> PendingPopupAlloc {
    Arc::new(Mutex::new(None))
}

/// Drain all pending popup-created events.
pub fn drain_popup_creates(sink: &PopupCreateSink) -> Vec<PopupCreated> {
    if let Ok(mut g) = sink.lock() {
        return g.drain(..).collect();
    }
    Vec::new()
}

/// Drain all pending popup-close browser ids.
pub fn drain_popup_closes(sink: &PopupCloseSink) -> Vec<i32> {
    if let Ok(mut g) = sink.lock() {
        return g.drain(..).collect();
    }
    Vec::new()
}

/// Pin the CEF runtime API version before any CEF entry point.
///
/// MUST be invoked before `cef::execute_process` / `cef::initialize`
/// in every process — both the browser binary and any helper.
pub fn init_cef_api() {
    let _ = cef::api_hash(cef::sys::CEF_API_VERSION_LAST, 0);
}

/// Execute the CEF subprocess entry point. Returns the process exit
/// code for child processes (renderer/GPU/utility), or -1 for the
/// browser process.
///
/// Call `init_cef_api()` before this.
pub fn execute_subprocess() -> i32 {
    init_cef_api();
    let args = cef::args::Args::new();
    let mut app = BuffrApp::new();
    cef::execute_process(
        Some(args.as_main_args()),
        Some(&mut app),
        std::ptr::null_mut(),
    )
}

/// Initialize CEF with neutral configuration.
///
/// # Arguments
///
/// - `cache_path` — root cache directory for the CEF profile.
/// - `app` — the [`BuffrApp`] instance to pass as the `CefApp`.
///
/// Returns `Ok(())` on success. Returns an error string when CEF's
/// `initialize` function returns anything other than 1.
///
/// macOS dev-mode path overrides (`browser_subprocess_path`,
/// `framework_dir_path`, `resources_dir_path`) are applied internally
/// when `cfg!(target_os = "macos")` and the executable is not inside
/// an `.app` bundle.
pub fn cef_initialize(cache_path: &str, app: &mut cef::App) -> Result<(), String> {
    let args = cef::args::Args::new();
    // `mut` only used in the macOS cfg block below; suppress
    // unused_mut warning on Linux/Windows where the bindings are immutable.
    #[allow(unused_mut)]
    let mut settings = cef::Settings {
        no_sandbox: 1,
        multi_threaded_message_loop: 0,
        root_cache_path: cef::CefString::from(cache_path),
        windowless_rendering_enabled: 1,
        ..Default::default()
    };

    // macOS: external_message_pump + binary/framework paths for cargo-run.
    #[cfg(target_os = "macos")]
    {
        settings.external_message_pump = 1;
        if let Ok(exe) = std::env::current_exe() {
            // Inside a real .app bundle the path already contains "Contents";
            // skip the cargo-run framework override.
            if !exe.components().any(|c| c.as_os_str() == "Contents") {
                if let Some(exe_dir) = exe.parent() {
                    let fw = exe_dir.join("../Frameworks/Chromium Embedded Framework.framework");
                    if let Ok(fw) = fw.canonicalize() {
                        let res = fw.join("Resources");
                        settings.browser_subprocess_path =
                            cef::CefString::from(exe.to_string_lossy().as_ref());
                        settings.framework_dir_path =
                            cef::CefString::from(fw.to_string_lossy().as_ref());
                        settings.resources_dir_path =
                            cef::CefString::from(res.to_string_lossy().as_ref());
                    }
                }
            }
        }
    }

    let ok = cef::initialize(
        Some(args.as_main_args()),
        Some(&settings),
        Some(app),
        std::ptr::null_mut(),
    );
    if ok != 1 {
        return Err(format!("cef::initialize returned {ok} (expected 1)"));
    }
    Ok(())
}

/// Shut CEF down cleanly. Call after all browsers are closed and CEF
/// message-loop work is done.
pub fn cef_shutdown() {
    cef::shutdown();
}

/// Pump one iteration of the CEF message loop.
///
/// On macOS this is a no-op when `cfg!(not(target_os = "macos"))` —
/// use [`pump_cef_message_loop_macos`] for platform-conditional pumping.
pub fn do_message_loop_work() {
    cef::do_message_loop_work();
}

/// Wipe all cookies via CEF's global cookie manager. The actual deletion
/// runs asynchronously on the IO thread; call before `cef_shutdown()`.
pub fn delete_all_cookies() {
    let Some(manager) = cef::cookie_manager_get_global_manager(None) else {
        tracing::warn!("delete_all_cookies: cookie_manager_get_global_manager returned None");
        return;
    };
    use cef::ImplCookieManager;
    let submitted = manager.delete_cookies(None, None, None);
    if submitted == 0 {
        tracing::warn!("delete_all_cookies: delete_cookies returned 0 (synchronous failure)");
    } else {
        tracing::info!("delete_all_cookies: delete dispatched");
    }
    let _ = manager.flush_store(None);
}

/// Run `cef::execute_process` for the subprocess dispatch in single-binary
/// mode. Returns the exit code for child processes (>= 0) or -1 for the
/// browser process. Call ONLY when `--type=` is present in argv.
pub fn execute_process_for_subprocess() -> i32 {
    init_cef_api();
    let args = cef::args::Args::new();
    let mut app = BuffrApp::new();
    cef::execute_process(
        Some(args.as_main_args()),
        Some(&mut app),
        std::ptr::null_mut(),
    )
}

/// Load the CEF framework shared library on platforms that require it.
///
/// On **macOS** the `Chromium Embedded Framework.framework` must be
/// loaded explicitly via cef-rs's `LibraryLoader` before any CEF entry
/// point.  `exe` is the path to the current executable; `is_helper`
/// controls whether the loader resolves the framework via
/// `../../..` (helper) or `../Frameworks` (browser process).
///
/// On **Linux / Windows** CEF links dynamically through `build.rs` so
/// this function is a no-op (returns `Ok(())`).
///
/// # Errors
///
/// Returns an error string when the macOS loader reports failure.
pub fn load_cef_library(exe: &std::path::Path, is_helper: bool) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let loader = cef::library_loader::LibraryLoader::new(exe, is_helper);
        if !loader.load() {
            return Err(format!(
                "CEF LibraryLoader failed (exe={}, is_helper={is_helper})",
                exe.display()
            ));
        }
        // Keep the loader alive for the lifetime of the process —
        // `Drop` calls `unload_library`, which we only want at exit.
        std::mem::forget(loader);
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = (exe, is_helper); // suppress unused warnings
    }

    Ok(())
}
