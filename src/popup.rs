//! Engine-agnostic popup-browser types.
//!
//! These types were previously defined in `buffr-cef` but are engine-neutral
//! — they only reference `SharedOsrFrame` / `SharedOsrViewState` from this
//! crate plus standard library types. Moving them here lets the `BrowserEngine`
//! trait expose popup sink accessors without pulling in a CEF dependency.
//!
//! Phase 6a (#95): moved from `buffr_cef::lib`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::{SharedOsrFrame, SharedOsrViewState};

// Re-export `PopupCreated` from `tab` so callers can use one import path.
pub use crate::tab::PopupCreated;

/// URLs queued by `LifeSpanHandler::on_before_popup` for dispositions
/// that should open as a new tab (`NEW_FOREGROUND_TAB`,
/// `NEW_BACKGROUND_TAB`). `NEW_POPUP` / `NEW_WINDOW` are not enqueued.
pub type PopupQueue = Arc<Mutex<VecDeque<String>>>;

/// Queue of popup-created events.
pub type PopupCreateSink = Arc<Mutex<VecDeque<PopupCreated>>>;

/// Queue of `browser_id` values for closed popup browsers.
pub type PopupCloseSink = Arc<Mutex<VecDeque<i32>>>;

/// Single-slot pending popup alloc: allocated by `on_before_popup`,
/// consumed by `on_after_created`.
pub type PendingPopupAlloc = Arc<Mutex<Option<(SharedOsrFrame, SharedOsrViewState, String)>>>;

pub fn new_popup_queue() -> PopupQueue {
    Arc::new(Mutex::new(VecDeque::new()))
}

pub fn new_popup_create_sink() -> PopupCreateSink {
    Arc::new(Mutex::new(VecDeque::new()))
}

pub fn new_popup_close_sink() -> PopupCloseSink {
    Arc::new(Mutex::new(VecDeque::new()))
}

pub fn new_pending_popup_alloc() -> PendingPopupAlloc {
    Arc::new(Mutex::new(None))
}

/// Drain all queued popup URL strings (new-tab re-routes).
pub fn drain_popup_urls(q: &PopupQueue) -> Vec<String> {
    if let Ok(mut g) = q.lock() {
        return g.drain(..).collect();
    }
    Vec::new()
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
