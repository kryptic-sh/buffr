//! CEF integration and browser host for buffr.
//!
//! Phase 1 surface: a [`BuffrApp`] wrapper around the `cef` crate's
//! `App` trait and a [`BrowserHost`] that creates a single browser
//! attached to a winit-backed native window. Both are intentionally
//! minimal — Phase 2 will expand them to wire up the modal engine
//! and render-process IPC.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use directories::ProjectDirs;
use thiserror::Error;

pub mod app;
pub mod cmdline;
pub mod crash;
pub mod cursor;
pub mod download_notice;
pub mod edit;
pub mod favicon;
pub mod find;
pub mod handlers;
pub mod hint;
pub mod host;
pub mod new_tab;
pub mod open_finder;
pub mod permissions;
pub mod scripts;
pub mod telemetry;
pub mod updates;

/// Off-screen rendering support. Linux and macOS use OSR; Windows uses
/// native windowed embedding.
pub mod osr;
pub use osr::{OsrFrame, OsrViewState, PopupFrameMap, SharedOsrFrame, SharedOsrViewState};

pub use app::{
    BuffrApp, ProfilePaths, force_renderer_accessibility_enabled, set_force_renderer_accessibility,
    take_scheduled_message_pump_delay_ms,
};
pub use crash::{CrashError, CrashReport, CrashReporter};
pub use cursor::{CursorState, SharedCursorState};
pub use download_notice::{
    DownloadNotice, DownloadNoticeKind, DownloadNoticeQueue, expire_stale as expire_stale_notices,
    new_queue as new_download_notice_queue, peek_front as peek_download_notice,
    pop_front as pop_download_notice, push as push_download_notice,
    queue_len as download_notice_queue_len,
};
pub use edit::{
    EDIT_CONSOLE_SENTINEL, EDIT_DOM_OVERLAY_CLASS, EditConsoleEvent, EditEventSink, EditFieldKind,
    ParseError as EditParseError, build_inject_script as build_edit_inject_script,
    drain_edit_events, new_edit_event_sink,
};
pub use favicon::{FaviconSink, FaviconUpdate, drain_favicon_updates, new_favicon_sink};
pub use find::{
    FindResult, FindResultSink, new_sink as new_find_sink, take_latest as take_find_result,
};
pub use hint::{
    DEFAULT_HINT_ALPHABET, DEFAULT_HINT_SELECTORS, HINT_CONSOLE_SENTINEL, HINT_OVERLAY_CLASS,
    HINT_OVERLAY_Z_INDEX, Hint, HintAction, HintAlphabet, HintConsoleEvent, HintError,
    HintEventSink, HintKind, HintLabel, HintRect, HintSession, build_inject_script,
    new_hint_event_sink, parse_console_event, take_hint_event,
};
pub use host::{BrowserHost, HintStatus, HostMode, Tab, TabId, TabSession, TabSummary};

/// URLs queued by `LifeSpanHandler::on_before_popup` for dispositions
/// that should open as a new tab (`NEW_FOREGROUND_TAB`,
/// `NEW_BACKGROUND_TAB` — typically `target="_blank"` and Ctrl+click).
/// `NEW_POPUP` / `NEW_WINDOW` (OAuth, `window.open` with features) are
/// not enqueued — CEF handles them natively.
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

/// A popup browser window that has been created and is ready to render.
///
/// Emitted by `BuffrLifeSpanHandler::on_after_created` when a popup browser
/// (created by `window.open` / `NEW_POPUP` disposition) comes into existence.
/// The apps layer drains these each tick and spawns a corresponding winit
/// window for each.
pub struct PopupCreated {
    /// CEF `Browser::identifier()` for the new popup browser.
    pub browser_id: i32,
    /// Initial URL (from `on_before_popup`'s `target_url`). May be empty
    /// if CEF didn't report one.
    pub url: String,
    /// OSR frame buffer shared with `OsrPaintHandler`. The apps layer
    /// reads from this to blit the popup's pixels.
    pub frame: SharedOsrFrame,
    /// OSR viewport state. The apps layer writes width/height here on
    /// window resize; `OsrPaintHandler::view_rect` reads them.
    pub view: SharedOsrViewState,
}

/// Thread-safe queue of popup-created events. Produced by
/// `BuffrLifeSpanHandler::on_after_created`; consumed by the apps layer.
pub type PopupCreateSink = Arc<Mutex<VecDeque<PopupCreated>>>;

/// Thread-safe queue of `browser_id` values for popups that have been
/// closed. Produced by `BuffrLifeSpanHandler::on_before_close`; the apps
/// layer drops the corresponding winit window.
pub type PopupCloseSink = Arc<Mutex<VecDeque<i32>>>;

pub fn new_popup_create_sink() -> PopupCreateSink {
    Arc::new(Mutex::new(VecDeque::new()))
}

pub fn new_popup_close_sink() -> PopupCloseSink {
    Arc::new(Mutex::new(VecDeque::new()))
}

/// Single-slot pending popup allocation. Set by `on_before_popup`
/// (before the browser id is known), consumed by `on_after_created`.
pub type PendingPopupAlloc = Arc<Mutex<Option<(SharedOsrFrame, SharedOsrViewState, String)>>>;

pub fn new_pending_popup_alloc() -> PendingPopupAlloc {
    Arc::new(Mutex::new(None))
}

/// Drain all pending popup-created events from `sink`. Returns an empty
/// `Vec` when the mutex is poisoned.
pub fn drain_popup_creates(sink: &PopupCreateSink) -> Vec<PopupCreated> {
    if let Ok(mut g) = sink.lock() {
        return g.drain(..).collect();
    }
    Vec::new()
}

/// Drain all pending popup-close browser ids from `sink`. Returns an empty
/// `Vec` when the mutex is poisoned.
pub fn drain_popup_closes(sink: &PopupCloseSink) -> Vec<i32> {
    if let Ok(mut g) = sink.lock() {
        return g.drain(..).collect();
    }
    Vec::new()
}

pub use new_tab::{
    NEW_TAB_HTML_TEMPLATE, NEW_TAB_KEYBINDS_MARKER, NEW_TAB_URL, NewTabHtmlProvider,
    register_buffr_handler_factory, register_buffr_handler_factory_static, register_buffr_scheme,
};
pub use permissions::{
    PendingPermission, PermissionsQueue, PromptOutcome, capabilities_for_media_mask,
    capabilities_for_request_mask, drain_with_defer as drain_permissions_with_defer,
    new_queue as new_permissions_queue, peek_front as peek_permission_front,
    pop_front as pop_permission_front, precheck as precheck_permission,
    queue_len as permissions_queue_len,
};
pub use telemetry::{
    KEY_APP_STARTS, KEY_BOOKMARKS_ADDED, KEY_DOWNLOADS_COMPLETED, KEY_PAGES_LOADED,
    KEY_SEARCHES_RUN, KEY_TABS_OPENED, TelemetryError, UsageCounters,
};
pub use updates::{
    DEFAULT_CHANNEL, DEFAULT_CHECK_INTERVAL_HOURS, DEFAULT_GITHUB_REPO, HttpClient, ReleaseInfo,
    UpdateChecker, UpdateConfig, UpdateError, UpdateStatus, UreqClient,
};

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("cef initialize() returned a failure code")]
    InitFailed,
    #[error("could not resolve project directories")]
    NoProjectDirs,
    #[error("invalid url: {0}")]
    InvalidUrl(String),
    #[error("creating browser failed")]
    CreateBrowserFailed,
}

/// `crates/buffr-core` version (`CARGO_PKG_VERSION`).
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Pin the CEF runtime API version before any CEF entry point.
///
/// `cef-rs` 147 wraps libcef 147, which in turn ships an API-version
/// negotiation layer: every C-to-C++ wrapper checks an integer
/// "API version" field on the wrapped struct (`App`, `Client`, …) and
/// aborts with `CefXxx_0_CToCpp called with invalid version -1` if it
/// hasn't been initialized.
///
/// `cef::api_hash(version, 0)` performs that negotiation; the `version`
/// is sticky after the first call. We call it with `CEF_API_VERSION_LAST`
/// (the highest version cef-dll-sys was generated against) so all
/// wrapper entries route through versioned slots instead of the bogus
/// slot-0 path.
///
/// MUST be invoked **before** `cef::execute_process` / `cef::initialize`
/// in every process — both the browser binary and any helper that
/// re-enters CEF for renderer/GPU/utility subprocess dispatch.
pub fn init_cef_api() {
    let _ = cef::api_hash(cef::sys::CEF_API_VERSION_LAST, 0);
}

/// Resolve buffr's per-user profile + cache directories. Created on
/// first call.
pub fn profile_paths() -> Result<ProfilePaths, CoreError> {
    let dirs = ProjectDirs::from("sh", "kryptic", "buffr").ok_or(CoreError::NoProjectDirs)?;
    let cache: PathBuf = dirs.cache_dir().to_path_buf();
    let data: PathBuf = dirs.data_dir().to_path_buf();
    let _ = std::fs::create_dir_all(&cache);
    let _ = std::fs::create_dir_all(&data);
    Ok(ProfilePaths { cache, data })
}
