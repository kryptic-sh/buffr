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

/// Take everything currently queued in a shared `VecDeque`, leaving it
/// empty. A poisoned lock yields an empty `Vec` — a producer thread
/// that panicked mid-push must not take the UI thread down with it.
///
/// Every queue drain in this crate goes through here (see
/// [`crate::permissions::drain_queue`] too); the named wrappers below
/// exist because other crates call them by name.
pub(crate) fn drain<T>(q: &Arc<Mutex<VecDeque<T>>>) -> Vec<T> {
    match q.lock() {
        Ok(mut g) => g.drain(..).collect(),
        Err(_) => Vec::new(),
    }
}

/// Drain all queued popup URL strings (new-tab re-routes).
pub fn drain_popup_urls(q: &PopupQueue) -> Vec<String> {
    drain(q)
}

/// Drain all pending popup-created events.
pub fn drain_popup_creates(sink: &PopupCreateSink) -> Vec<PopupCreated> {
    drain(sink)
}

/// Drain all pending popup-close browser ids.
pub fn drain_popup_closes(sink: &PopupCloseSink) -> Vec<i32> {
    drain(sink)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_takes_everything_and_leaves_queue_empty() {
        let q = new_popup_queue();
        {
            let mut g = q.lock().unwrap();
            g.push_back("https://a.example".to_string());
            g.push_back("https://b.example".to_string());
        }
        let urls = drain_popup_urls(&q);
        assert_eq!(urls, ["https://a.example", "https://b.example"]);
        assert!(q.lock().unwrap().is_empty());
        assert!(drain_popup_urls(&q).is_empty(), "second drain is empty");
    }

    #[test]
    fn drain_close_ids_preserves_order() {
        let sink = new_popup_close_sink();
        {
            let mut g = sink.lock().unwrap();
            g.push_back(7);
            g.push_back(9);
        }
        assert_eq!(drain_popup_closes(&sink), vec![7, 9]);
    }

    #[test]
    fn drain_of_poisoned_queue_is_empty_not_a_panic() {
        let q = new_popup_queue();
        q.lock().unwrap().push_back("https://a.example".to_string());
        let poisoner = Arc::clone(&q);
        let _ = std::thread::spawn(move || {
            let _g = poisoner.lock().unwrap();
            panic!("poison the mutex");
        })
        .join();
        assert!(q.is_poisoned());
        assert!(drain_popup_urls(&q).is_empty());
    }
}
