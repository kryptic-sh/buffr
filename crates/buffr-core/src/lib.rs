//! Engine-agnostic browser shell for buffr.
//!
//! After Phase 1 this crate holds the non-CEF modules: event sinks,
//! the hint/edit DOM-IPC layer, context-menu model, favicon cache,
//! telemetry, updates, crash reporting, and the idle inhibit abstraction.
//!
//! CEF-specific code lives in `buffr-cef`. The `BrowserEngine` trait
//! lives in `buffr-engine`.

use thiserror::Error;

pub mod cmdline;
pub mod console_nonce;
mod console_sentinel;
pub mod context_menu;
pub mod crash;
pub mod cursor;
pub mod download_notice;
pub mod edit;
pub mod favicon;
pub mod favicon_cache;
pub mod find;
pub mod hint;
pub mod html;
pub mod http;
pub mod image_copy;
pub mod inhibit;
pub mod media_probe;
pub mod open_finder;
pub mod private_net;
pub mod scripts;
pub mod telemetry;
pub mod updates;

pub use console_nonce::{CONSOLE_NONCE_LEN, ConsoleNonces, new_console_nonce};
pub use context_menu::{
    CONTEXT_MENU_REQUEST_QUEUE_CAP, ContextMenuItem, ContextMenuRequest, ContextMenuSink,
    ContextMenuTarget, build_model as build_context_menu_model,
    build_tab_model as build_tab_context_menu_model, new_context_menu_sink,
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
pub use favicon::{
    FaviconEnabled, FaviconSink, FaviconUpdate, drain_favicon_updates, favicon_is_enabled,
    new_favicon_enabled, new_favicon_sink, set_favicon_enabled,
};
pub use favicon_cache::{CachedFavicon, FaviconCache, FaviconCacheError, origin_of};
pub use find::{
    FindResult, FindResultSink, new_sink as new_find_sink, take_latest as take_find_result,
};
pub use hint::{
    DEFAULT_HINT_ALPHABET, DEFAULT_HINT_SELECTORS, HINT_CONSOLE_SENTINEL, Hint, HintAction,
    HintAlphabet, HintConsoleEvent, HintError, HintEventSink, HintKind, HintRect, HintSession,
    build_inject_script, new_hint_event_sink, parse_console_event, take_hint_event,
};
pub use inhibit::{IdleInhibitor, InhibitError, new_inhibitor};
pub use telemetry::{
    KEY_APP_STARTS, KEY_BOOKMARKS_ADDED, KEY_DOWNLOADS_COMPLETED, KEY_PAGES_LOADED,
    KEY_SEARCHES_RUN, KEY_TABS_OPENED, TelemetryError, UsageCounters,
};
pub use updates::{
    HttpClient, ReleaseInfo, UpdateChecker, UpdateConfig, UpdateError, UpdateStatus, UreqClient,
};

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("engine initialization failed")]
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
