//! Find-in-page support — a one-slot mailbox between CEF's
//! `FindHandler::on_find_result` callback (called on the browser
//! thread) and the buffr UI thread.
//!
//! Phase 3 contract:
//!
//! - `BrowserHost::start_find` calls `BrowserHost::host().find(...)`
//!   and stashes the query so subsequent `FindNext` / `FindPrev`
//!   actions reuse it.
//! - CEF invokes our [`crate::handlers::BuffrFindHandler::on_find_result`]
//!   on every match-list update; the handler writes the latest
//!   [`FindResult`] into a `Mutex<Option<FindResult>>`.
//! - The UI thread polls the sink each frame (`take_latest`) and
//!   updates the statusline.
//!
//! The sink is a one-slot mailbox rather than a queue because find
//! results are idempotent — only the latest update matters for the
//! statusline. Older ticks are dropped silently.

use std::sync::{Arc, Mutex};

/// Snapshot of the most recent CEF find callback. CEF emits these as
/// a stream during a search; consumers care about the latest only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FindResult {
    /// Total matches on the page.
    pub count: u32,
    /// 1-based index of the active match. `0` while CEF is still
    /// locating the first match.
    pub current: u32,
    /// `true` once CEF has finished computing the match list.
    pub final_update: bool,
}

/// One-slot mailbox shared by the find handler and the UI thread.
///
/// Cloning is cheap (an `Arc` increment); both sides hold their own
/// clone. `lock()` is non-async — the critical section is
/// "overwrite an `Option`", which never blocks meaningfully.
pub type FindResultSink = Arc<Mutex<Option<FindResult>>>;

/// Construct a fresh, empty sink.
pub fn new_sink() -> FindResultSink {
    Arc::new(Mutex::new(None))
}

/// Consume the latest find result. Returns `Some(result)` exactly once
/// per write — clears the slot on read so the UI thread can detect
/// "is there a new tick to render?". Returns `None` when no tick has
/// arrived since the last call.
pub fn take_latest(sink: &FindResultSink) -> Option<FindResult> {
    sink.lock().ok().and_then(|mut guard| guard.take())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn take_latest_empty_sink_returns_none() {
        let sink = new_sink();
        assert_eq!(take_latest(&sink), None);
    }

    #[test]
    fn take_latest_consumes_last_write() {
        let sink = new_sink();
        {
            let mut guard = sink.lock().unwrap();
            *guard = Some(FindResult {
                count: 3,
                current: 1,
                final_update: false,
            });
        }
        let r = take_latest(&sink).expect("sink populated");
        assert_eq!(r.count, 3);
        assert_eq!(r.current, 1);
        assert!(!r.final_update);
        // Slot is empty after read.
        assert_eq!(take_latest(&sink), None);
    }

    #[test]
    fn writes_overwrite_old_unread_result() {
        let sink = new_sink();
        {
            let mut g = sink.lock().unwrap();
            *g = Some(FindResult {
                count: 1,
                current: 1,
                final_update: false,
            });
        }
        {
            let mut g = sink.lock().unwrap();
            *g = Some(FindResult {
                count: 5,
                current: 2,
                final_update: true,
            });
        }
        let r = take_latest(&sink).unwrap();
        assert_eq!(r.count, 5);
        assert_eq!(r.current, 2);
        assert!(r.final_update);
    }
}
