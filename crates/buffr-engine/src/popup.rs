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

/// Where a re-routed popup should land relative to the user's attention.
///
/// The disposition the engine reports is the *only* thing that separates a
/// Ctrl+click from a `target="_blank"` click — both arrive as "open a new
/// tab". Dropping it makes the two indistinguishable, which is how every
/// re-routed popup ended up stealing focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopupFocus {
    /// `NEW_FOREGROUND_TAB` — `target="_blank"`, `window.open`, or a
    /// middle-ground gesture the engine already resolved to foreground.
    /// The user asked to go there, so follow the tab.
    Foreground,
    /// `NEW_BACKGROUND_TAB` — Ctrl/Cmd+click. The user is queueing
    /// something to read later and expects to stay where they are.
    Background,
}

impl PopupFocus {
    /// True when the new tab should become the active one.
    pub fn is_foreground(self) -> bool {
        matches!(self, PopupFocus::Foreground)
    }
}

/// A popup re-routed to a tab by `LifeSpanHandler::on_before_popup`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PopupTarget {
    pub url: String,
    pub focus: PopupFocus,
}

/// Popups queued by `LifeSpanHandler::on_before_popup` for dispositions
/// that should open as a new tab (`NEW_FOREGROUND_TAB`,
/// `NEW_BACKGROUND_TAB`). `NEW_POPUP` / `NEW_WINDOW` are not enqueued.
pub type PopupQueue = Arc<Mutex<VecDeque<PopupTarget>>>;

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

/// Drain all queued popup re-routes, each carrying where it should land.
pub fn drain_popup_targets(q: &PopupQueue) -> Vec<PopupTarget> {
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

    fn target(url: &str, focus: PopupFocus) -> PopupTarget {
        PopupTarget {
            url: url.to_string(),
            focus,
        }
    }

    #[test]
    fn drain_takes_everything_and_leaves_queue_empty() {
        let q = new_popup_queue();
        {
            let mut g = q.lock().unwrap();
            g.push_back(target("https://a.example", PopupFocus::Foreground));
            g.push_back(target("https://b.example", PopupFocus::Background));
        }
        let got = drain_popup_targets(&q);
        assert_eq!(
            got,
            vec![
                target("https://a.example", PopupFocus::Foreground),
                target("https://b.example", PopupFocus::Background),
            ]
        );
        assert!(q.lock().unwrap().is_empty());
        assert!(drain_popup_targets(&q).is_empty(), "second drain is empty");
    }

    /// The whole point of carrying the disposition: a Ctrl+click and a
    /// `target="_blank"` click must not come out of the queue looking the
    /// same. Before this, both were bare strings and both stole focus.
    #[test]
    fn background_and_foreground_survive_the_queue_distinctly() {
        let q = new_popup_queue();
        {
            let mut g = q.lock().unwrap();
            // Ctrl+click, then a _blank link, then window.open.
            g.push_back(target("https://ctrl.example", PopupFocus::Background));
            g.push_back(target("https://blank.example", PopupFocus::Foreground));
            g.push_back(target("https://jsopen.example", PopupFocus::Foreground));
        }
        let got = drain_popup_targets(&q);
        let focused: Vec<&str> = got
            .iter()
            .filter(|t| t.focus.is_foreground())
            .map(|t| t.url.as_str())
            .collect();
        let backgrounded: Vec<&str> = got
            .iter()
            .filter(|t| !t.focus.is_foreground())
            .map(|t| t.url.as_str())
            .collect();
        assert_eq!(focused, ["https://blank.example", "https://jsopen.example"]);
        assert_eq!(backgrounded, ["https://ctrl.example"]);
    }

    #[test]
    fn is_foreground_matches_the_variant() {
        assert!(PopupFocus::Foreground.is_foreground());
        assert!(!PopupFocus::Background.is_foreground());
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
        q.lock()
            .unwrap()
            .push_back(target("https://a.example", PopupFocus::Foreground));
        let poisoner = Arc::clone(&q);
        let _ = std::thread::spawn(move || {
            let _g = poisoner.lock().unwrap();
            panic!("poison the mutex");
        })
        .join();
        assert!(q.is_poisoned());
        assert!(drain_popup_targets(&q).is_empty());
    }
}
