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

/// Clone the front entry (without removing it), if any.
///
/// Unlike [`peek_front`] this keeps the `resolve_id`, so a UI layer can
/// remember *which* request it put on screen — see [`PromptIdentity`].
pub fn peek_front_entry(queue: &PermissionsQueue) -> Option<PendingPermission> {
    let g = queue.lock().ok()?;
    g.front().cloned()
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

// ── Prompt identity ───────────────────────────────────────────────────────────

/// Identity of the queue entry a UI layer currently has on screen.
///
/// The queue is shared: backends push onto it from another thread, and a
/// backend may *withdraw* an entry it has cancelled itself (CEF's
/// `OnDismissPermissionPrompt` retains the queue by `resolve_id` when a tab
/// navigates away). So "the entry the user is looking at" and "the front of
/// the queue" are not the same thing, and an answer must only ever be applied
/// to the former.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptIdentity {
    /// The backend minted a `resolve_id`. This is an exact, unforgeable
    /// identity — matched by string equality and nothing else.
    Id(String),
    /// The backend supplied no `resolve_id` (synchronous backends that do not
    /// need async resolution). There is no token to match on, so identity
    /// falls back to the two fields the prompt actually renders: origin and
    /// capabilities. A different request that is indistinguishable on screen
    /// is therefore treated as the same request — the user's answer still
    /// lands on the origin and capabilities they read. An entry that *does*
    /// carry a `resolve_id` never matches this variant.
    Untracked {
        /// Origin as rendered in the prompt.
        origin: String,
        /// Capabilities as rendered in the prompt.
        capabilities: Vec<Capability>,
    },
}

impl PromptIdentity {
    /// Identity of `pending`, as a UI layer should remember it.
    pub fn of(pending: &PendingPermission) -> Self {
        match &pending.resolve_id {
            Some(id) => PromptIdentity::Id(id.clone()),
            None => PromptIdentity::Untracked {
                origin: pending.origin.clone(),
                capabilities: pending.capabilities.clone(),
            },
        }
    }

    /// Does `pending` denote the same request as this identity?
    pub fn matches(&self, pending: &PendingPermission) -> bool {
        match self {
            PromptIdentity::Id(id) => pending.resolve_id.as_deref() == Some(id.as_str()),
            PromptIdentity::Untracked {
                origin,
                capabilities,
            } => {
                pending.resolve_id.is_none()
                    && &pending.origin == origin
                    && &pending.capabilities == capabilities
            }
        }
    }
}

/// Which entry a user's prompt answer belongs to.
#[derive(Debug, Clone)]
pub enum ResolveTarget {
    /// The front of the queue is the entry that was on screen — apply the
    /// outcome to it.
    Apply(PendingPermission),
    /// The entry that was on screen is gone (withdrawn by the backend, or
    /// never recorded). The answer belongs to nothing: it must not be applied
    /// to whatever is at the front now, because the user never saw it.
    Stale,
}

/// Decide which entry an answer applies to, given the current queue front and
/// the identity of the entry the UI displayed.
///
/// Pure: no locking, no side effects. `Apply` is returned **only** when the
/// front is the very entry `shown` identifies. Everything else — an empty
/// queue, a different request at the front, or a UI that never recorded what
/// it displayed — is [`ResolveTarget::Stale`].
pub fn resolve_target(
    queue_front: Option<&PendingPermission>,
    shown: Option<&PromptIdentity>,
) -> ResolveTarget {
    match (queue_front, shown) {
        (Some(front), Some(id)) if id.matches(front) => ResolveTarget::Apply(front.clone()),
        _ => ResolveTarget::Stale,
    }
}

/// Lock `queue`, and pop the front **only** if it is the entry `shown`
/// identifies.
///
/// The check and the pop happen under one lock so a concurrent backend
/// withdrawal cannot slip a different request into the front between them.
pub fn take_front_matching(
    queue: &PermissionsQueue,
    shown: Option<&PromptIdentity>,
) -> ResolveTarget {
    let Ok(mut g) = queue.lock() else {
        return ResolveTarget::Stale;
    };
    match resolve_target(g.front(), shown) {
        ResolveTarget::Apply(_) => match g.pop_front() {
            Some(front) => ResolveTarget::Apply(front),
            None => ResolveTarget::Stale,
        },
        ResolveTarget::Stale => ResolveTarget::Stale,
    }
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

    // ── Prompt identity / resolve targeting ──────────────────────────────────

    fn pending(origin: &str, caps: &[Capability], id: Option<&str>) -> PendingPermission {
        PendingPermission {
            origin: origin.to_string(),
            capabilities: caps.to_vec(),
            resolve_id: id.map(str::to_string),
        }
    }

    /// Normal single-request flow: what was shown is what is at the front.
    #[test]
    fn resolve_target_applies_when_front_is_the_shown_entry() {
        let front = pending("https://a.com", &[Capability::Camera], Some("id-a"));
        let shown = PromptIdentity::of(&front);
        match resolve_target(Some(&front), Some(&shown)) {
            ResolveTarget::Apply(p) => {
                assert_eq!(p.origin, "https://a.com");
                assert_eq!(p.capabilities, vec![Capability::Camera]);
                assert_eq!(p.resolve_id.as_deref(), Some("id-a"));
            }
            ResolveTarget::Stale => panic!("expected Apply for the entry that was on screen"),
        }
    }

    /// Regression: page A's prompt is on screen, A is withdrawn by the
    /// backend, B is now the front. The answer must NOT land on B.
    #[test]
    fn resolve_target_is_stale_when_front_is_a_different_request() {
        let shown_entry = pending("https://a.com", &[Capability::Camera], Some("id-a"));
        let shown = PromptIdentity::of(&shown_entry);
        let front = pending("https://b.com", &[Capability::Microphone], Some("id-b"));
        assert!(
            matches!(
                resolve_target(Some(&front), Some(&shown)),
                ResolveTarget::Stale
            ),
            "answer for id-a must not be applied to id-b"
        );
    }

    /// Same origin, same id, different capabilities is still the same request
    /// — but a *reused-looking* origin with a different id is not.
    #[test]
    fn resolve_target_is_stale_for_same_origin_different_id() {
        let shown_entry = pending("https://a.com", &[Capability::Camera], Some("id-1"));
        let shown = PromptIdentity::of(&shown_entry);
        let front = pending("https://a.com", &[Capability::Camera], Some("id-2"));
        assert!(matches!(
            resolve_target(Some(&front), Some(&shown)),
            ResolveTarget::Stale
        ));
    }

    #[test]
    fn resolve_target_is_stale_when_queue_is_empty() {
        let shown_entry = pending("https://a.com", &[Capability::Camera], Some("id-a"));
        let shown = PromptIdentity::of(&shown_entry);
        assert!(matches!(
            resolve_target(None, Some(&shown)),
            ResolveTarget::Stale
        ));
    }

    #[test]
    fn resolve_target_is_stale_when_nothing_was_recorded_as_shown() {
        let front = pending("https://a.com", &[Capability::Camera], Some("id-a"));
        assert!(matches!(
            resolve_target(Some(&front), None),
            ResolveTarget::Stale
        ));
    }

    /// `resolve_id: None` rule — identity is origin + capabilities.
    #[test]
    fn resolve_target_untracked_matches_on_origin_and_caps() {
        let front = pending("https://a.com", &[Capability::Camera], None);
        let shown = PromptIdentity::of(&front);
        assert!(matches!(shown, PromptIdentity::Untracked { .. }));
        match resolve_target(Some(&front), Some(&shown)) {
            ResolveTarget::Apply(p) => assert_eq!(p.origin, "https://a.com"),
            ResolveTarget::Stale => panic!("untracked entry should match itself"),
        }

        // Different origin → stale.
        let other = pending("https://evil.com", &[Capability::Camera], None);
        assert!(matches!(
            resolve_target(Some(&other), Some(&shown)),
            ResolveTarget::Stale
        ));
        // Same origin, different capabilities → stale.
        let other_caps = pending("https://a.com", &[Capability::Microphone], None);
        assert!(matches!(
            resolve_target(Some(&other_caps), Some(&shown)),
            ResolveTarget::Stale
        ));
        // An id-bearing entry never matches an untracked identity.
        let tracked = pending("https://a.com", &[Capability::Camera], Some("id-a"));
        assert!(matches!(
            resolve_target(Some(&tracked), Some(&shown)),
            ResolveTarget::Stale
        ));
    }

    /// An untracked identity must not answer a tracked prompt either way.
    #[test]
    fn resolve_target_tracked_identity_rejects_untracked_front() {
        let shown_entry = pending("https://a.com", &[Capability::Camera], Some("id-a"));
        let shown = PromptIdentity::of(&shown_entry);
        let front = pending("https://a.com", &[Capability::Camera], None);
        assert!(matches!(
            resolve_target(Some(&front), Some(&shown)),
            ResolveTarget::Stale
        ));
    }

    #[test]
    fn take_front_matching_pops_only_the_shown_entry() {
        let q = new_queue();
        let a = pending("https://a.com", &[Capability::Camera], Some("id-a"));
        let b = pending("https://b.com", &[Capability::Microphone], Some("id-b"));
        q.lock().unwrap().push_back(a.clone());
        q.lock().unwrap().push_back(b.clone());
        let shown = PromptIdentity::of(&a);

        match take_front_matching(&q, Some(&shown)) {
            ResolveTarget::Apply(p) => assert_eq!(p.resolve_id.as_deref(), Some("id-a")),
            ResolveTarget::Stale => panic!("front is the shown entry"),
        }
        assert_eq!(queue_len(&q), 1);

        // A is gone; answering "A" again must not consume or apply to B.
        assert!(matches!(
            take_front_matching(&q, Some(&shown)),
            ResolveTarget::Stale
        ));
        assert_eq!(queue_len(&q), 1, "B must stay queued and unanswered");
        assert_eq!(
            peek_front_entry(&q).unwrap().resolve_id.as_deref(),
            Some("id-b")
        );
    }

    #[test]
    fn take_front_matching_leaves_queue_untouched_when_stale() {
        // Exactly the reported scenario: A withdrawn, B at the front.
        let q = new_queue();
        let a = pending("https://a.com", &[Capability::Camera], Some("id-a"));
        let b = pending("https://b.com", &[Capability::Microphone], Some("id-b"));
        q.lock().unwrap().push_back(b.clone());
        let shown = PromptIdentity::of(&a);
        assert!(matches!(
            take_front_matching(&q, Some(&shown)),
            ResolveTarget::Stale
        ));
        assert_eq!(queue_len(&q), 1);
    }

    #[test]
    fn peek_front_entry_keeps_resolve_id_and_does_not_consume() {
        let q = new_queue();
        q.lock().unwrap().push_back(pending(
            "https://a.com",
            &[Capability::Camera],
            Some("id-a"),
        ));
        let front = peek_front_entry(&q).unwrap();
        assert_eq!(front.resolve_id.as_deref(), Some("id-a"));
        assert_eq!(queue_len(&q), 1);
        assert!(peek_front_entry(&new_queue()).is_none());
    }

    #[test]
    fn prompt_identity_of_picks_variant_by_resolve_id() {
        let tracked = pending("https://a.com", &[Capability::Camera], Some("id-a"));
        assert_eq!(
            PromptIdentity::of(&tracked),
            PromptIdentity::Id("id-a".to_string())
        );
        let untracked = pending("https://a.com", &[Capability::Camera], None);
        assert_eq!(
            PromptIdentity::of(&untracked),
            PromptIdentity::Untracked {
                origin: "https://a.com".to_string(),
                capabilities: vec![Capability::Camera],
            }
        );
    }
}
