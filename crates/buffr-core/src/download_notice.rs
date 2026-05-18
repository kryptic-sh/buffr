//! Download notification queue — pure data, no UI deps.
//!
//! Defines the [`DownloadNotice`] type that travels from the CEF
//! `DownloadHandler` callbacks (on the CEF IO thread) to the UI thread's
//! render loop via a shared `Mutex<VecDeque<DownloadNotice>>`.
//!
//! The queue shape mirrors [`crate::permissions`] deliberately: the same
//! `Arc<Mutex<VecDeque<_>>>` pattern works here because notices are
//! passive (no callback to fire) and expire automatically by wall-clock
//! age rather than by user acknowledgement.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// One download notification surfaced in the chrome strip.
#[derive(Debug, Clone)]
pub struct DownloadNotice {
    /// What happened.
    pub kind: DownloadNoticeKind,
    /// Suggested filename (from the URL / Content-Disposition).
    pub filename: String,
    /// Absolute path on disk (may be empty for `Started`/`Failed`).
    pub path: String,
    /// When this notice was created — drives auto-expiry.
    pub created_at: Instant,
}

/// Download lifecycle event that drives icon + accent colour selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadNoticeKind {
    /// File transfer has begun; `default_dir` chosen, no dialog.
    Started,
    /// Transfer finished and the file is on disk.
    Completed,
    /// Transfer was cancelled or interrupted.
    Failed,
}

impl DownloadNoticeKind {
    /// How long this notice stays visible before the drain loop pops it.
    pub fn expiry_duration(self) -> Duration {
        match self {
            // Started: brief — the Completed notice lands shortly after.
            DownloadNoticeKind::Started => Duration::from_secs(2),
            // Completed / Failed: give the user 4 s to read.
            DownloadNoticeKind::Completed | DownloadNoticeKind::Failed => Duration::from_secs(4),
        }
    }
}

impl DownloadNotice {
    /// Returns `true` when the notice has lived past its expiry window.
    pub fn is_expired(&self) -> bool {
        self.created_at.elapsed() > self.kind.expiry_duration()
    }
}

/// Shared queue between the CEF IO thread and the UI render loop.
pub type DownloadNoticeQueue = Arc<Mutex<VecDeque<DownloadNotice>>>;

/// Build a fresh empty download-notice queue.
pub fn new_queue() -> DownloadNoticeQueue {
    Arc::new(Mutex::new(VecDeque::new()))
}

/// Push a new notice onto the back of the queue.
pub fn push(queue: &DownloadNoticeQueue, notice: DownloadNotice) {
    if let Ok(mut g) = queue.lock() {
        g.push_back(notice);
    }
}

/// Peek at the front notice without removing it.
pub fn peek_front(queue: &DownloadNoticeQueue) -> Option<DownloadNotice> {
    queue.lock().ok().and_then(|g| g.front().cloned())
}

/// Remove and return the front notice.
pub fn pop_front(queue: &DownloadNoticeQueue) -> Option<DownloadNotice> {
    queue.lock().ok().and_then(|mut g| g.pop_front())
}

/// Number of notices currently queued.
pub fn queue_len(queue: &DownloadNoticeQueue) -> usize {
    queue.lock().map(|g| g.len()).unwrap_or(0)
}

/// Pop and discard any notices that have exceeded their expiry window.
/// Returns the number of notices dropped.
pub fn expire_stale(queue: &DownloadNoticeQueue) -> usize {
    let Ok(mut g) = queue.lock() else { return 0 };
    let before = g.len();
    g.retain(|n| !n.is_expired());
    before - g.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_starts_empty() {
        let q = new_queue();
        assert_eq!(queue_len(&q), 0);
        assert!(peek_front(&q).is_none());
        assert!(pop_front(&q).is_none());
    }

    #[test]
    fn push_and_peek_and_pop() {
        let q = new_queue();
        push(
            &q,
            DownloadNotice {
                kind: DownloadNoticeKind::Started,
                filename: "file.txt".into(),
                path: "/tmp/file.txt".into(),
                created_at: Instant::now(),
            },
        );
        assert_eq!(queue_len(&q), 1);
        let n = peek_front(&q).unwrap();
        assert_eq!(n.filename, "file.txt");
        // peek leaves it there.
        assert_eq!(queue_len(&q), 1);
        let n2 = pop_front(&q).unwrap();
        assert_eq!(n2.filename, "file.txt");
        assert_eq!(queue_len(&q), 0);
    }

    #[test]
    fn expire_stale_drops_old_notices() {
        let q = new_queue();
        // Force expiry by using an ancient created_at.
        push(
            &q,
            DownloadNotice {
                kind: DownloadNoticeKind::Completed,
                filename: "old.zip".into(),
                path: "/tmp/old.zip".into(),
                // 10 seconds ago — well past the 4 s expiry.
                created_at: Instant::now() - Duration::from_secs(10),
            },
        );
        push(
            &q,
            DownloadNotice {
                kind: DownloadNoticeKind::Started,
                filename: "fresh.zip".into(),
                path: String::new(),
                created_at: Instant::now(),
            },
        );
        assert_eq!(queue_len(&q), 2);
        let dropped = expire_stale(&q);
        assert_eq!(dropped, 1);
        assert_eq!(queue_len(&q), 1);
        let n = peek_front(&q).unwrap();
        assert_eq!(n.filename, "fresh.zip");
    }

    #[test]
    fn started_expiry_shorter_than_completed() {
        assert!(
            DownloadNoticeKind::Started.expiry_duration()
                < DownloadNoticeKind::Completed.expiry_duration()
        );
    }
}
