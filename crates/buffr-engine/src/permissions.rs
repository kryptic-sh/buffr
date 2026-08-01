//! Neutral permission-prompt types shared between all engine backends.
//!
//! Phase 8a (#88): moved here from `buffr_cef::permissions` so that
//! non-CEF backends can populate the same queue without taking a
//! dependency on CEF-specific types.
//!
//! # Queue model
//!
//! A single [`PermissionsQueue`] lives on the apps layer. Backends push
//! [`PendingPermission`] entries onto it.  When the user answers the prompt,
//! the apps layer calls [`BrowserEngine::resolve_permission`] with the
//! `resolve_id` taken from [`PendingPermission::resolve_id`].
//!
//! - **CEF backend**: on push, stores the original C++ callback in a
//!   per-engine `HashMap<String, CefCallback>`. On resolve, looks up
//!   the callback by `resolve_id` and fires it.
//!
//! # Thread safety
//!
//! The queue is `Arc<Mutex<VecDeque<…>>>` so CEF's IO thread can push
//! while the UI thread drains.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use buffr_permissions::Capability;

// ── Core types ────────────────────────────────────────────────────────────────

/// Outcome the UI thread reports when resolving a permission prompt.
///
/// Moved from `buffr_cef::permissions` in Phase 8a (#88). `buffr-cef`
/// re-exports this so existing callers at `buffr_cef::PromptOutcome` keep
/// working.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptOutcome {
    /// Allow this request. Optionally persist the decision for this origin.
    Allow {
        /// `true` → write a sticky `Allow` row to the permissions store.
        remember: bool,
    },
    /// Deny this request. Optionally persist.
    Deny {
        /// `true` → write a sticky `Deny` row to the permissions store.
        remember: bool,
    },
    /// Defer — treat as deny-once. No decision is persisted.
    Defer,
}

/// One pending permission request, in a backend-neutral form.
///
/// The `resolve_id` token links back to the backend-internal state
/// (CEF callback reference or CDP JS promise id). Apps code is not
/// expected to interpret it; just pass it back to
/// [`BrowserEngine::resolve_permission`].
#[derive(Debug, Clone)]
pub struct PendingPermission {
    /// Origin the page is requesting permissions for (e.g.
    /// `"https://example.com"`). Shown in the prompt strip.
    pub origin: String,
    /// Capabilities this request is asking about (camera, mic, …).
    pub capabilities: Vec<Capability>,
    /// Backend-internal token. `None` for backends that don't need
    /// async resolution (e.g. future synchronous backends). CDP uses a
    /// UUID string; CEF maps to an internal callback registry entry.
    pub resolve_id: Option<String>,
}

/// Shared queue between the backend (push) and the UI thread (drain).
///
/// Alias for `Arc<Mutex<VecDeque<PendingPermission>>>`.
pub type PermissionsQueue = Arc<Mutex<VecDeque<PendingPermission>>>;

// ── Queue helpers ─────────────────────────────────────────────────────────────

/// Build a fresh empty permissions queue.
pub fn new_queue() -> PermissionsQueue {
    Arc::new(Mutex::new(VecDeque::new()))
}

/// Number of pending requests currently in `queue`.
pub fn queue_len(queue: &PermissionsQueue) -> usize {
    queue.lock().map(|g| g.len()).unwrap_or(0)
}

/// Pop the front of the queue, if any.
pub fn pop_front(queue: &PermissionsQueue) -> Option<PendingPermission> {
    queue.lock().ok().and_then(|mut g| g.pop_front())
}

/// Inspect (without removing) the front of the queue.
///
/// Returns `(origin, capabilities)` so the UI can render the strip
/// without touching backend-internal state.
pub fn peek_front(queue: &PermissionsQueue) -> Option<(String, Vec<Capability>)> {
    let g = queue.lock().ok()?;
    let front = g.front()?;
    Some((front.origin.clone(), front.capabilities.clone()))
}

/// Drop every entry in `queue`, treating them as deferred (denied once).
///
/// Callers should pair this with `BrowserEngine::resolve_permission` to
/// fire the appropriate backend-side callbacks. The default drain just
/// discards the neutral entries — callers that need callback-side cleanup
/// must drain manually.
pub fn drain_queue(queue: &PermissionsQueue) -> Vec<PendingPermission> {
    crate::popup::drain(queue)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use buffr_permissions::Capability;

    #[test]
    fn queue_starts_empty() {
        let q = new_queue();
        assert_eq!(queue_len(&q), 0);
        assert!(pop_front(&q).is_none());
        assert!(peek_front(&q).is_none());
    }

    #[test]
    fn push_and_peek_front() {
        let q = new_queue();
        q.lock().unwrap().push_back(PendingPermission {
            origin: "https://example.com".to_string(),
            capabilities: vec![Capability::Geolocation],
            resolve_id: Some("id-1".to_string()),
        });
        assert_eq!(queue_len(&q), 1);
        let (origin, caps) = peek_front(&q).unwrap();
        assert_eq!(origin, "https://example.com");
        assert_eq!(caps, vec![Capability::Geolocation]);
        // peek does not consume
        assert_eq!(queue_len(&q), 1);
    }

    #[test]
    fn pop_front_consumes() {
        let q = new_queue();
        q.lock().unwrap().push_back(PendingPermission {
            origin: "https://a.com".to_string(),
            capabilities: vec![Capability::Camera, Capability::Microphone],
            resolve_id: None,
        });
        let p = pop_front(&q).unwrap();
        assert_eq!(p.origin, "https://a.com");
        assert_eq!(p.capabilities.len(), 2);
        assert!(pop_front(&q).is_none());
    }

    #[test]
    fn drain_queue_empties() {
        let q = new_queue();
        for i in 0..3u32 {
            q.lock().unwrap().push_back(PendingPermission {
                origin: format!("https://site{i}.com"),
                capabilities: vec![Capability::Notifications],
                resolve_id: Some(format!("id-{i}")),
            });
        }
        assert_eq!(queue_len(&q), 3);
        let drained = drain_queue(&q);
        assert_eq!(drained.len(), 3);
        assert_eq!(queue_len(&q), 0);
    }

    #[test]
    fn prompt_outcome_allow_remember_fields() {
        let o = PromptOutcome::Allow { remember: true };
        assert!(matches!(o, PromptOutcome::Allow { remember: true }));
        let o2 = PromptOutcome::Allow { remember: false };
        assert!(matches!(o2, PromptOutcome::Allow { remember: false }));
    }

    #[test]
    fn prompt_outcome_deny_fields() {
        let o = PromptOutcome::Deny { remember: false };
        assert!(matches!(o, PromptOutcome::Deny { remember: false }));
    }

    #[test]
    fn prompt_outcome_defer() {
        let o = PromptOutcome::Defer;
        assert!(matches!(o, PromptOutcome::Defer));
    }
}
