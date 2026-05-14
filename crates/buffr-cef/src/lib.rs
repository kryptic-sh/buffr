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
pub use host::{BrowserHost, ClipboardReader, HintStatus, Tab, TabId, TabSession, TabSummary};
pub use new_tab::{
    NEW_TAB_HTML_TEMPLATE, NEW_TAB_KEYBINDS_MARKER, NEW_TAB_SPLASH_ART_MARKER, NEW_TAB_URL,
    NewTabHtmlProvider, register_buffr_handler_factory, register_buffr_handler_factory_static,
    register_buffr_scheme,
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
