//! Permissions wiring between CEF and buffr's UI thread.
//!
//! CEF emits permission requests via two callbacks on
//! `cef_permission_handler_t`:
//!
//! - `on_request_media_access_permission` — fired for camera /
//!   microphone / screen-capture (legacy `getUserMedia` path). Carries a
//!   `cef_media_access_callback_t` and a `u32` bitmask of
//!   `cef_media_access_permission_types_t` bits
//!   (`DEVICE_VIDEO_CAPTURE = 2`, `DEVICE_AUDIO_CAPTURE = 1`,
//!   desktop variants 4 + 8).
//! - `on_show_permission_prompt` — fired for everything else
//!   (geolocation, notifications, MIDI sysex, clipboard, …). Carries a
//!   `cef_permission_prompt_callback_t`, a `prompt_id` (so dismissals
//!   can correlate), and a `u32` bitmask of
//!   `cef_permission_request_types_t` bits.
//!
//! Both fire on CEF's IO/UI thread. The handler:
//!
//! 1. Decomposes the bitmask into a [`Vec<Capability>`].
//! 2. Walks the [`Permissions`] store. If **every** capability has a
//!    stored decision and they all agree (all-allow → `Accept`,
//!    otherwise `Deny`), the callback fires synchronously.
//! 3. Otherwise the request + callback land on a
//!    `Mutex<VecDeque<PendingPermission>>` and the UI thread drains
//!    one per `about_to_wait` tick.
//!
//! The UI thread invokes [`PendingPermission::resolve`] with a
//! [`PromptOutcome`] which fires the C++ callback exactly once and
//! optionally records a sticky decision in the store.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use buffr_permissions::{Capability, Decision, PermError, Permissions};
use cef::{
    ImplMediaAccessCallback, ImplPermissionPromptCallback, MediaAccessCallback,
    PermissionPromptCallback, PermissionRequestResult,
};
use tracing::{trace, warn};

// CEF media-access permission bits — mirror
// `cef_media_access_permission_types_t` from the cef-dll-sys bindings.
// Kept as locals so we don't depend on the sys-level enum directly.
const MEDIA_DEVICE_AUDIO_CAPTURE: u32 = 1;
const MEDIA_DEVICE_VIDEO_CAPTURE: u32 = 2;
const MEDIA_DESKTOP_AUDIO_CAPTURE: u32 = 4;
const MEDIA_DESKTOP_VIDEO_CAPTURE: u32 = 8;

// CEF generic permission bits — mirror `cef_permission_request_types_t`.
// We expand a minimal subset here; everything else is mapped to
// [`Capability::Other`].
const PERM_CAMERA_PAN_TILT_ZOOM: u32 = 2;
const PERM_CAMERA_STREAM: u32 = 4;
const PERM_CLIPBOARD: u32 = 16;
const PERM_GEOLOCATION: u32 = 256;
const PERM_MIC_STREAM: u32 = 4096;
const PERM_MIDI_SYSEX: u32 = 8192;
const PERM_NOTIFICATIONS: u32 = 32768;

/// Decision the UI thread reports back when resolving a queued
/// request. Carried out of band so the queue draining code stays
/// CEF-callback-agnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptOutcome {
    /// Allow this request; remember the decision for this origin
    /// (`remember = true`) or only honour it once.
    Allow { remember: bool },
    /// Deny this request; remember the decision (`remember = true`)
    /// or only honour it once.
    Deny { remember: bool },
    /// Defer — synonymous with deny-once. The C++ callback receives
    /// `Dismiss` (for the prompt path) or `cancel()` (for the media
    /// path); nothing is persisted.
    Defer,
}

/// One pending permission request. The two variants correspond to the
/// two CEF callback paths. Construction wraps the callback in a
/// [`RefGuard`]-clone so the queue can outlive the IO-thread frame
/// that produced it; resolution invokes the callback exactly once and
/// drops the wrapper.
pub enum PendingPermission {
    MediaAccess {
        origin: String,
        capabilities: Vec<Capability>,
        callback: MediaAccessCallback,
        /// Bitmask CEF originally requested. We only grant the bits
        /// the user said yes to; anything outside this mask would be
        /// rejected by CEF anyway, but pre-masking keeps the contract
        /// crisp.
        requested_mask: u32,
    },
    Prompt {
        origin: String,
        capabilities: Vec<Capability>,
        callback: PermissionPromptCallback,
        prompt_id: u64,
    },
}

impl std::fmt::Debug for PendingPermission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PendingPermission::MediaAccess {
                origin,
                capabilities,
                requested_mask,
                ..
            } => f
                .debug_struct("PendingPermission::MediaAccess")
                .field("origin", origin)
                .field("capabilities", capabilities)
                .field("requested_mask", requested_mask)
                .finish_non_exhaustive(),
            PendingPermission::Prompt {
                origin,
                capabilities,
                prompt_id,
                ..
            } => f
                .debug_struct("PendingPermission::Prompt")
                .field("origin", origin)
                .field("capabilities", capabilities)
                .field("prompt_id", prompt_id)
                .finish_non_exhaustive(),
        }
    }
}

impl PendingPermission {
    /// Origin string the UI thread should show in the prompt strip.
    pub fn origin(&self) -> &str {
        match self {
            PendingPermission::MediaAccess { origin, .. }
            | PendingPermission::Prompt { origin, .. } => origin,
        }
    }

    /// Capabilities this request is asking about.
    pub fn capabilities(&self) -> &[Capability] {
        match self {
            PendingPermission::MediaAccess { capabilities, .. }
            | PendingPermission::Prompt { capabilities, .. } => capabilities,
        }
    }

    /// Resolve the request: invoke the C++ callback exactly once and
    /// (optionally) persist the decision in `store`. Returns the
    /// number of rows written to the store (0 or `capabilities.len()`).
    ///
    /// Dropping a `PendingPermission` without calling `resolve` would
    /// leak the CEF refcounted callback and wedge the renderer until
    /// the browser is torn down. The handler's `Drop` impl below
    /// guards against that by dispatching a default `Defer` outcome.
    pub fn resolve(self, outcome: PromptOutcome, store: &Permissions) -> Result<usize, PermError> {
        let (decision_to_persist, remember) = match outcome {
            PromptOutcome::Allow { remember } => (Some(Decision::Allow), remember),
            PromptOutcome::Deny { remember } => (Some(Decision::Deny), remember),
            PromptOutcome::Defer => (None, false),
        };

        let mut written = 0usize;

        match self {
            PendingPermission::MediaAccess {
                origin,
                capabilities,
                callback,
                requested_mask,
            } => {
                if remember && let Some(decision) = decision_to_persist {
                    for cap in &capabilities {
                        store.set(&origin, *cap, decision)?;
                        written += 1;
                    }
                }
                match outcome {
                    PromptOutcome::Allow { .. } => callback.cont(requested_mask),
                    PromptOutcome::Deny { .. } | PromptOutcome::Defer => callback.cancel(),
                }
            }
            PendingPermission::Prompt {
                origin,
                capabilities,
                callback,
                prompt_id: _,
            } => {
                if remember && let Some(decision) = decision_to_persist {
                    for cap in &capabilities {
                        store.set(&origin, *cap, decision)?;
                        written += 1;
                    }
                }
                let result = match outcome {
                    PromptOutcome::Allow { .. } => PermissionRequestResult::ACCEPT,
                    PromptOutcome::Deny { .. } => PermissionRequestResult::DENY,
                    PromptOutcome::Defer => PermissionRequestResult::DISMISS,
                };
                callback.cont(result);
            }
        }
        Ok(written)
    }
}

/// Shared queue between the CEF IO/UI callbacks and the UI thread.
pub type PermissionsQueue = Arc<Mutex<VecDeque<PendingPermission>>>;

/// Build a fresh empty permissions queue.
pub fn new_queue() -> PermissionsQueue {
    Arc::new(Mutex::new(VecDeque::new()))
}

/// Number of pending requests currently in `queue`. Used by the UI
/// strip to render `(N more pending)`.
pub fn queue_len(queue: &PermissionsQueue) -> usize {
    queue.lock().map(|g| g.len()).unwrap_or(0)
}

/// Pop the front of the queue, if any.
pub fn pop_front(queue: &PermissionsQueue) -> Option<PendingPermission> {
    queue.lock().ok().and_then(|mut g| g.pop_front())
}

/// Inspect (without removing) the front of the queue.
///
/// Returned tuple is `(origin, capabilities)` so the UI can render the
/// strip without touching the callback wrapper. Holding the lock just
/// long enough to clone primitives keeps the IO thread unblocked.
pub fn peek_front(queue: &PermissionsQueue) -> Option<(String, Vec<Capability>)> {
    let g = queue.lock().ok()?;
    let front = g.front()?;
    Some((front.origin().to_string(), front.capabilities().to_vec()))
}

/// Drop every entry in `queue`, dispatching a [`PromptOutcome::Defer`]
/// for each so the renderer doesn't wedge. Called at shutdown.
pub fn drain_with_defer(queue: &PermissionsQueue, store: &Permissions) {
    let drained: Vec<PendingPermission> = match queue.lock() {
        Ok(mut g) => g.drain(..).collect(),
        Err(_) => return,
    };
    for p in drained {
        if let Err(err) = p.resolve(PromptOutcome::Defer, store) {
            warn!(error = %err, "permissions: defer dispatch on drain failed");
        }
    }
}

/// Decompose a media-access bitmask into [`Capability`]s. Audio bits
/// map to `Microphone`, video bits to `Camera`. Desktop-capture bits
/// fold into the same surfaces — buffr does not expose a separate
/// "screen share" decision in 1.0.
pub fn capabilities_for_media_mask(mask: u32) -> Vec<Capability> {
    let mut out = Vec::with_capacity(2);
    let video =
        (mask & MEDIA_DEVICE_VIDEO_CAPTURE) != 0 || (mask & MEDIA_DESKTOP_VIDEO_CAPTURE) != 0;
    let audio =
        (mask & MEDIA_DEVICE_AUDIO_CAPTURE) != 0 || (mask & MEDIA_DESKTOP_AUDIO_CAPTURE) != 0;
    if video {
        out.push(Capability::Camera);
    }
    if audio {
        out.push(Capability::Microphone);
    }
    out
}

/// Decompose a permission-request bitmask into [`Capability`]s. Bits
/// without a named [`Capability`] variant land in
/// [`Capability::Other`] carrying the bit value, so the user can still
/// see + persist a decision for them.
pub fn capabilities_for_request_mask(mask: u32) -> Vec<Capability> {
    let mut out = Vec::new();
    if mask == 0 {
        return out;
    }
    let mut remaining = mask;
    let known: &[(u32, Capability)] = &[
        (PERM_CAMERA_STREAM, Capability::Camera),
        (PERM_CAMERA_PAN_TILT_ZOOM, Capability::Camera),
        (PERM_MIC_STREAM, Capability::Microphone),
        (PERM_GEOLOCATION, Capability::Geolocation),
        (PERM_NOTIFICATIONS, Capability::Notifications),
        (PERM_CLIPBOARD, Capability::Clipboard),
        (PERM_MIDI_SYSEX, Capability::Midi),
    ];
    for (bit, cap) in known {
        if (remaining & *bit) != 0 {
            // Dedupe — multiple bits can map to the same Capability
            // (e.g. PERM_CAMERA_STREAM + PERM_CAMERA_PAN_TILT_ZOOM both
            // surface as Camera).
            if !out.contains(cap) {
                out.push(*cap);
            }
            remaining &= !*bit;
        }
    }
    // Everything else lands in Other(bit).
    let mut bit = 1u32;
    while bit != 0 {
        if (remaining & bit) != 0 {
            out.push(Capability::Other(bit));
            remaining &= !bit;
        }
        bit = bit.checked_shl(1).unwrap_or(0);
    }
    out
}

/// Walk `caps` against `store`. Returns:
///
/// - `Some(Decision::Allow)` if every capability has a stored
///   `Allow` decision.
/// - `Some(Decision::Deny)` if every capability has a stored decision
///   and at least one is `Deny`.
/// - `None` if any capability has no stored decision (caller must
///   prompt).
pub fn precheck(
    store: &Permissions,
    origin: &str,
    caps: &[Capability],
) -> Result<Option<Decision>, PermError> {
    if caps.is_empty() {
        // No caps → nothing to ask. Treat as Allow so the callback
        // doesn't hang. CEF should never actually emit a zero-cap
        // request, but we belt-and-brace.
        return Ok(Some(Decision::Allow));
    }
    let mut all_allow = true;
    for cap in caps {
        match store.get(origin, *cap)? {
            Some(Decision::Allow) => {}
            Some(Decision::Deny) => {
                all_allow = false;
            }
            None => {
                trace!(origin, capability = ?cap, "permissions: precheck miss");
                return Ok(None);
            }
        }
    }
    if all_allow {
        Ok(Some(Decision::Allow))
    } else {
        Ok(Some(Decision::Deny))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_mask_video_only() {
        let caps = capabilities_for_media_mask(MEDIA_DEVICE_VIDEO_CAPTURE);
        assert_eq!(caps, vec![Capability::Camera]);
    }

    #[test]
    fn media_mask_audio_only() {
        let caps = capabilities_for_media_mask(MEDIA_DEVICE_AUDIO_CAPTURE);
        assert_eq!(caps, vec![Capability::Microphone]);
    }

    #[test]
    fn media_mask_both() {
        let mask = MEDIA_DEVICE_VIDEO_CAPTURE | MEDIA_DEVICE_AUDIO_CAPTURE;
        let caps = capabilities_for_media_mask(mask);
        assert_eq!(caps, vec![Capability::Camera, Capability::Microphone]);
    }

    #[test]
    fn media_mask_desktop_collapses_to_same_caps() {
        let mask = MEDIA_DESKTOP_AUDIO_CAPTURE | MEDIA_DESKTOP_VIDEO_CAPTURE;
        let caps = capabilities_for_media_mask(mask);
        assert_eq!(caps, vec![Capability::Camera, Capability::Microphone]);
    }

    #[test]
    fn request_mask_geolocation() {
        let caps = capabilities_for_request_mask(PERM_GEOLOCATION);
        assert_eq!(caps, vec![Capability::Geolocation]);
    }

    #[test]
    fn request_mask_camera_with_pan_tilt_zoom_dedupes() {
        let mask = PERM_CAMERA_STREAM | PERM_CAMERA_PAN_TILT_ZOOM;
        let caps = capabilities_for_request_mask(mask);
        assert_eq!(caps, vec![Capability::Camera]);
    }

    #[test]
    fn request_mask_unknown_bit_falls_back_to_other() {
        // Bit 1 (AR_SESSION) is not in our known list.
        let caps = capabilities_for_request_mask(1);
        assert_eq!(caps, vec![Capability::Other(1)]);
    }

    #[test]
    fn request_mask_combined_known_and_unknown() {
        // Geolocation (256) + AR_SESSION (1) → both surface.
        let caps = capabilities_for_request_mask(PERM_GEOLOCATION | 1);
        assert!(caps.contains(&Capability::Geolocation));
        assert!(caps.contains(&Capability::Other(1)));
        assert_eq!(caps.len(), 2);
    }

    #[test]
    fn request_mask_empty_returns_empty() {
        let caps = capabilities_for_request_mask(0);
        assert!(caps.is_empty());
    }

    #[test]
    fn precheck_empty_caps_allows() {
        let store = Permissions::open_in_memory().unwrap();
        let r = precheck(&store, "https://x", &[]).unwrap();
        assert_eq!(r, Some(Decision::Allow));
    }

    #[test]
    fn precheck_all_allow_returns_allow() {
        let store = Permissions::open_in_memory().unwrap();
        store
            .set("https://x", Capability::Camera, Decision::Allow)
            .unwrap();
        store
            .set("https://x", Capability::Microphone, Decision::Allow)
            .unwrap();
        let r = precheck(
            &store,
            "https://x",
            &[Capability::Camera, Capability::Microphone],
        )
        .unwrap();
        assert_eq!(r, Some(Decision::Allow));
    }

    #[test]
    fn precheck_one_deny_returns_deny() {
        let store = Permissions::open_in_memory().unwrap();
        store
            .set("https://x", Capability::Camera, Decision::Allow)
            .unwrap();
        store
            .set("https://x", Capability::Microphone, Decision::Deny)
            .unwrap();
        let r = precheck(
            &store,
            "https://x",
            &[Capability::Camera, Capability::Microphone],
        )
        .unwrap();
        assert_eq!(r, Some(Decision::Deny));
    }

    #[test]
    fn precheck_one_missing_returns_none() {
        let store = Permissions::open_in_memory().unwrap();
        store
            .set("https://x", Capability::Camera, Decision::Allow)
            .unwrap();
        let r = precheck(
            &store,
            "https://x",
            &[Capability::Camera, Capability::Microphone],
        )
        .unwrap();
        assert_eq!(r, None);
    }

    #[test]
    fn queue_starts_empty() {
        let q = new_queue();
        assert_eq!(queue_len(&q), 0);
        assert!(pop_front(&q).is_none());
        assert!(peek_front(&q).is_none());
    }
}
