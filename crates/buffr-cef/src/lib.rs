//! CEF integration and browser host for buffr-engine.
//!
//! `buffr-cef` is the production browser backend for buffr.  It wraps
//! Chromium Embedded Framework (CEF) and implements the [`buffr_engine::BrowserEngine`]
//! trait so the apps layer can drive full Chromium rendering without knowing
//! any CEF-specific types.
//!
//! # Position in the workspace
//!
//! ```text
//! apps/buffr  ──►  buffr-engine (trait)
//!                       ▲
//!                  buffr-cef  (this crate — CEF backend)
//! ```
//!
//! All CEF-specific code lives here.  `buffr-core` and the apps layer depend
//! only on `buffr-engine`; this crate is selected at link time for production
//! builds.
//!
//! # Implements
//!
//! [`BrowserEngine`] methods fully covered:
//! - Tab lifecycle: `open_tab`, `close_tab`, `activate_tab`, `duplicate_active`,
//!   `reopen_closed_tab`
//! - Navigation: `navigate`, `go_back`, `go_forward`, `reload`, `stop`
//! - OSR: `osr_resize`, `osr_sleep`, `osr_invalidate_view`, `force_repaint_active`
//! - Input: `send_key_event`, `send_mouse_event`, `send_scroll_event`
//! - Edit ops: `frame_undo/redo/cut/copy/paste/paste_plain/del/select_all`
//! - Clipboard: `clipboard_handle`
//! - Permissions: full `drain_permissions_*` + `precheck_permission`
//! - Context menu: `build_model`, `build_tab_model`, `handle_menu_item`
//! - Find-in-page: `find_start`, `find_next`, `find_prev`, `find_close`
//! - Downloads: `start_download`, drain events
//! - Audio: `any_audio_active`, `drain_audio_events`, `run_media_probe`
//! - Dev tools: `show_dev_tools_at`
//! - Custom schemes: `register_buffr_src_scheme` (`buffr://` served via InternalServer HTTP)
//!
//! Deferred / unimplemented in this crate: none — CEF provides the full
//! surface.  Per-engine `CefRequestContext` isolation is a Phase 5+ follow-up
//! (see issue #74).
//!
//! # Phases
//!
//! The v0.9.0 capability surface adds:
//! - IME composition events wired to `CefBrowserHost::ImeSetComposition`
//! - PiP toggle via JS media helper (`media_picture_in_picture`)
//! - Scheme handler factories for `buffr-src:` (view-source) and `buffr://`
//!   (settings / new-tab)

pub mod app;
pub mod audio;
pub mod backend;
pub(crate) mod convert;
pub mod handlers;
pub mod host;
pub(crate) mod html;
pub mod new_tab;
pub mod osr;
pub mod permissions;
pub mod view_source_scheme;

pub use app::{
    BuffrApp, device_scale_factor, force_renderer_accessibility_enabled, profile_paths,
    set_device_scale_factor, set_force_renderer_accessibility,
    take_scheduled_message_pump_delay_ms,
};
pub use audio::{
    AudioEvent, AudioEventQueue, AudioState, AudioStateSink, any_audio_active, drain_audio_events,
    new_audio_event_queue, new_audio_state_sink,
};
pub use backend::{CefBackend, CefEngineSinks};
pub use host::{BrowserHost, ClipboardReader, Tab, TabSession};
// HintStatus moved to buffr-engine in Phase 6b (#95). Re-exported here so
// existing `buffr_cef::HintStatus` imports keep resolving without modification.
pub use buffr_engine::HintStatus;
// ProfilePaths moved to buffr-engine in Phase 6e (#95). Re-exported here so
// existing `buffr_cef::ProfilePaths` imports keep resolving without modification.
pub use buffr_engine::ProfilePaths;
// Re-export unified types from buffr-engine so callers using `buffr_cef::TabId`
// keep working without also importing buffr-engine directly.
pub use buffr_engine::{BrowserEngine, TabId, TabSummary};
pub use new_tab::{
    NEW_TAB_HTML_TEMPLATE, NEW_TAB_KEYBINDS_MARKER, NEW_TAB_SPLASH_ART_MARKER, NEW_TAB_URL,
    SETTINGS_URL,
};
pub use osr::{OsrFrame, OsrViewState, PopupFrameMap, SharedOsrFrame, SharedOsrViewState};
// Popup types and helpers promoted to buffr-engine in Phase 6a (#95).
// Re-exported here so existing `buffr_cef::PopupQueue` / `drain_popup_*`
// imports keep resolving without modification.
pub use buffr_engine::{
    PopupCloseSink, PopupCreateSink, PopupCreated, PopupQueue, drain_popup_closes,
    drain_popup_creates, drain_popup_targets, new_popup_close_sink, new_popup_create_sink,
    new_popup_queue,
};
pub use permissions::{
    CefCallbackRegistry, PendingPermission, PromptOutcome, capabilities_for_media_mask,
    capabilities_for_request_mask,
    drain_registry_with_defer as drain_permissions_registry_with_defer, enqueue_to_both,
    next_resolve_id, precheck as precheck_permission,
};
pub use view_source_scheme::{register_buffr_src_handler_factory, register_buffr_src_scheme};

/// Pin the CEF runtime API version before any CEF entry point.
///
/// MUST be invoked before `cef::execute_process` / `cef::initialize`
/// in every process — both the browser binary and any helper.
pub fn init_cef_api() {
    let _ = cef::api_hash(cef::sys::CEF_API_VERSION_LAST, 0);
}

/// Execute the CEF subprocess entry point. Returns the process exit
/// code for child processes (renderer/GPU/utility), or -1 for the
/// browser process. Call ONLY when `--type=` is present in argv (the
/// single-binary dispatch) or from the dedicated helper binary.
///
/// Calls `init_cef_api()` internally; calling it beforehand is harmless.
///
/// L17: this used to have a byte-identical twin named
/// `execute_process_for_subprocess`. The twin is gone; `CefBackend::execute_subprocess`
/// now calls this.
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
        multi_threaded_message_loop: 0,
        root_cache_path: cef::CefString::from(cache_path),
        windowless_rendering_enabled: 1,
        ..Default::default()
    };

    // macOS: external_message_pump + binary/framework paths for cargo-run.
    #[cfg(target_os = "macos")]
    {
        settings.external_message_pump = 1;
        // Inside a real .app bundle the path already contains "Contents";
        // skip the cargo-run framework override.
        if let Ok(exe) = std::env::current_exe()
            && !exe.components().any(|c| c.as_os_str() == "Contents")
            && let Some(exe_dir) = exe.parent()
            && let Ok(fw) = exe_dir
                .join("../Frameworks/Chromium Embedded Framework.framework")
                .canonicalize()
        {
            let res = fw.join("Resources");
            settings.browser_subprocess_path = cef::CefString::from(exe.to_string_lossy().as_ref());
            settings.framework_dir_path = cef::CefString::from(fw.to_string_lossy().as_ref());
            settings.resources_dir_path = cef::CefString::from(res.to_string_lossy().as_ref());
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
