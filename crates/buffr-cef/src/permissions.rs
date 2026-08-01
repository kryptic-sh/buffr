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
//! 3. Otherwise the request + callback land on both:
//!    a. The CEF-internal [`CefPermissionsQueue`] (for callback resolution)
//!    b. The neutral [`buffr_engine::PermissionsQueue`] (for the apps UI)
//!
//! The UI thread calls [`BrowserEngine::resolve_permission`] which pops
//! from the callback registry and fires the C++ callback exactly once,
//! optionally recording a sticky decision in the store.
//!
//! # Phase 8a (#88) changes
//!
//! - [`PromptOutcome`] is now re-exported from `buffr_engine::permissions`.
//!   The type alias below keeps existing `buffr_cef::PromptOutcome` imports
//!   working.
//! - [`CefPermissionsQueue`] is the CEF-internal queue (C++ callbacks).
//! - The neutral `buffr_engine::PermissionsQueue` is the queue the apps
//!   layer drains to drive the prompt strip.
//! - [`CefCallbackRegistry`] maps `resolve_id → PendingPermission` for
//!   async resolution from the UI thread.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use buffr_permissions::{Capability, Decision, PermError, Permissions};
use cef::{
    ImplMediaAccessCallback, ImplPermissionPromptCallback, MediaAccessCallback,
    PermissionPromptCallback, PermissionRequestResult,
};
use tracing::{trace, warn};

// Re-export PromptOutcome from buffr-engine so existing
// `buffr_cef::PromptOutcome` callers keep working without modification.
pub use buffr_engine::permissions::PromptOutcome;

/// Atomic counter for generating unique resolve IDs for each permission
/// request. The ID is formatted as `"cef-<n>"`.
static CEF_RESOLVE_ID: AtomicU64 = AtomicU64::new(1);

/// Generate a unique resolve ID for a new CEF permission request.
pub fn next_resolve_id() -> String {
    format!("cef-{}", CEF_RESOLVE_ID.fetch_add(1, Ordering::Relaxed))
}

/// Registry mapping `resolve_id` → CEF [`PendingPermission`] (with C++
/// callbacks). Held by `BrowserHost`; written on the CEF IO thread,
/// drained by the UI thread via [`BrowserEngine::resolve_permission`].
pub type CefCallbackRegistry = Arc<Mutex<std::collections::HashMap<String, PendingPermission>>>;

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

/// One pending permission request. The two variants correspond to the
/// two CEF callback paths. Construction wraps the callback in a
/// [`RefGuard`]-clone so the queue can outlive the IO-thread frame
/// that produced it; resolution invokes the callback exactly once and
/// drops the wrapper.
pub enum PendingPermission {
    MediaAccess {
        origin: String,
        capabilities: Vec<Capability>,
        /// `Some` until the callback has been fired (or explicitly
        /// disarmed). [`Drop`] uses this to guarantee the C++ callback
        /// is invoked exactly once — see the impl below.
        callback: Option<MediaAccessCallback>,
        /// Bitmask CEF originally requested. We only grant the bits
        /// the user said yes to; anything outside this mask would be
        /// rejected by CEF anyway, but pre-masking keeps the contract
        /// crisp.
        requested_mask: u32,
    },
    Prompt {
        origin: String,
        capabilities: Vec<Capability>,
        /// `Some` until the callback has been fired (or explicitly
        /// disarmed). See [`Drop`] below.
        callback: Option<PermissionPromptCallback>,
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
    /// The C++ callback is **always** invoked, even when the store write
    /// fails — a sqlite error (disk full, locked db) must never leave the
    /// renderer wedged waiting on a `MediaAccessCallback` that never
    /// fires. The store error is captured and returned *after* the
    /// callback has been dispatched.
    ///
    /// Dropping a `PendingPermission` without calling `resolve` would
    /// otherwise leak the CEF refcounted callback and wedge the renderer
    /// until the browser is torn down. The [`Drop`] impl below guards
    /// against that by dispatching a `cancel()` / `DISMISS` outcome.
    /// Call [`Self::disarm`] first when CEF has already retired the
    /// callback on its own (see `on_dismiss_permission_prompt`).
    pub fn resolve(
        mut self,
        outcome: PromptOutcome,
        store: &Permissions,
    ) -> Result<usize, PermError> {
        let (decision_to_persist, remember) = decision_for(outcome);

        let mut written = 0usize;
        let mut store_err: Option<PermError> = None;

        match &mut self {
            PendingPermission::MediaAccess {
                origin,
                capabilities,
                callback,
                requested_mask,
            } => {
                if remember && let Some(decision) = decision_to_persist {
                    (written, store_err) = persist_decisions(store, origin, capabilities, decision);
                }
                // ALWAYS fire the callback, even if the store write above
                // failed. `Option::take` makes this a no-op for the `Drop`
                // impl below, so the callback runs exactly once.
                if let Some(cb) = callback.take() {
                    match outcome {
                        PromptOutcome::Allow { .. } => cb.cont(*requested_mask),
                        PromptOutcome::Deny { .. } | PromptOutcome::Defer => cb.cancel(),
                    }
                }
            }
            PendingPermission::Prompt {
                origin,
                capabilities,
                callback,
                prompt_id: _,
            } => {
                if remember && let Some(decision) = decision_to_persist {
                    (written, store_err) = persist_decisions(store, origin, capabilities, decision);
                }
                let result = match outcome {
                    PromptOutcome::Allow { .. } => PermissionRequestResult::ACCEPT,
                    PromptOutcome::Deny { .. } => PermissionRequestResult::DENY,
                    PromptOutcome::Defer => PermissionRequestResult::DISMISS,
                };
                if let Some(cb) = callback.take() {
                    cb.cont(result);
                }
            }
        }
        match store_err {
            Some(err) => Err(err),
            None => Ok(written),
        }
    }

    /// Drop the C++ callback handle **without** invoking it.
    ///
    /// Only correct when CEF has already retired the callback on its own —
    /// today that is `on_dismiss_permission_prompt`, where CEF has
    /// cancelled the prompt and calling `cont()` again would be a
    /// double-invoke. Everything else must go through [`Self::resolve`]
    /// (or let [`Drop`] fire the default outcome).
    pub fn disarm(&mut self) {
        match self {
            PendingPermission::MediaAccess { callback, .. } => {
                let _ = callback.take();
            }
            PendingPermission::Prompt { callback, .. } => {
                let _ = callback.take();
            }
        }
    }
}

/// Map a [`PromptOutcome`] to `(decision_to_persist, remember)`.
fn decision_for(outcome: PromptOutcome) -> (Option<Decision>, bool) {
    match outcome {
        PromptOutcome::Allow { remember } => (Some(Decision::Allow), remember),
        PromptOutcome::Deny { remember } => (Some(Decision::Deny), remember),
        PromptOutcome::Defer => (None, false),
    }
}

/// Write `decision` for every capability in `caps`.
///
/// Returns `(rows_written, first_error)`. Never short-circuits with `?` —
/// the caller must still fire the CEF callback regardless of a store
/// failure, so the error is handed back instead of propagated (H9).
fn persist_decisions(
    store: &Permissions,
    origin: &str,
    caps: &[Capability],
    decision: Decision,
) -> (usize, Option<PermError>) {
    let mut written = 0usize;
    for cap in caps {
        match store.set(origin, *cap, decision) {
            Ok(()) => written += 1,
            Err(err) => return (written, Some(err)),
        }
    }
    (written, None)
}

/// Guarantee the CEF callback is invoked exactly once.
///
/// If a `PendingPermission` is dropped while still armed — registry drop
/// at shutdown, a poisoned mutex, an early `return` on an error path —
/// the C++ side would otherwise wait forever and wedge the renderer.
/// Fire the most conservative outcome (`cancel()` / `DISMISS`) instead.
impl Drop for PendingPermission {
    fn drop(&mut self) {
        match self {
            PendingPermission::MediaAccess {
                origin, callback, ..
            } => {
                if let Some(cb) = callback.take() {
                    warn!(
                        %origin,
                        "permissions: media-access request dropped unresolved — cancelling"
                    );
                    cb.cancel();
                }
            }
            PendingPermission::Prompt {
                origin, callback, ..
            } => {
                if let Some(cb) = callback.take() {
                    warn!(
                        %origin,
                        "permissions: prompt request dropped unresolved — dismissing"
                    );
                    cb.cont(PermissionRequestResult::DISMISS);
                }
            }
        }
    }
}

/// Push `pending` onto both the CEF callback registry and the neutral
/// engine queue.
///
/// - `registry`: the `CefCallbackRegistry` owned by `BrowserHost`; the
///   CEF-specific `pending` is stored keyed by `resolve_id`.
/// - `engine_queue`: the neutral [`buffr_engine::PermissionsQueue`]
///   that the apps layer drains to show the prompt strip.
/// - `resolve_id`: opaque string returned to the UI thread via the
///   neutral `PendingPermission::resolve_id` field.
pub fn enqueue_to_both(
    pending: PendingPermission,
    registry: &CefCallbackRegistry,
    engine_queue: &buffr_engine::PermissionsQueue,
    resolve_id: String,
) {
    let neutral = buffr_engine::permissions::PendingPermission {
        origin: pending.origin().to_string(),
        capabilities: pending.capabilities().to_vec(),
        resolve_id: Some(resolve_id.clone()),
    };
    // Push to callback registry first (IO thread).
    match registry.lock() {
        Ok(mut reg) => {
            reg.insert(resolve_id, pending);
        }
        Err(_) => {
            warn!("permissions: callback registry mutex poisoned");
            return;
        }
    }
    // Push neutral entry to the engine queue.
    match engine_queue.lock() {
        Ok(mut q) => q.push_back(neutral),
        Err(_) => {
            warn!("permissions: engine queue mutex poisoned");
        }
    }
}

/// Drain the [`CefCallbackRegistry`], firing `Defer` for each pending
/// callback so the renderer doesn't wedge.
///
/// Called from `BrowserHost::close_all_browsers` (L16 wiring) so pending
/// callbacks are retired while CEF's threads are still running, rather than
/// waiting for the registry `Arc` to reach refcount zero somewhere inside
/// `cef::shutdown()`.
pub fn drain_registry_with_defer(registry: &CefCallbackRegistry, store: &Permissions) {
    let drained: Vec<PendingPermission> = match registry.lock() {
        Ok(mut reg) => reg.drain().map(|(_, v)| v).collect(),
        Err(_) => return,
    };
    for p in drained {
        if let Err(err) = p.resolve(PromptOutcome::Defer, store) {
            warn!(error = %err, "permissions: defer dispatch on registry drain failed");
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

    // ── H9 regression coverage ────────────────────────────────────────────
    //
    // `PendingPermission::resolve` itself needs a live `MediaAccessCallback`
    // (a C++ refcounted object), so it cannot be exercised here. The two
    // pure pieces it was split into can be — and they are the parts that
    // encode the H9 contract: `persist_decisions` must NOT short-circuit
    // with `?`, it must hand the error back so the caller can still fire
    // the CEF callback.

    #[test]
    fn decision_for_maps_outcomes() {
        assert_eq!(
            decision_for(PromptOutcome::Allow { remember: true }),
            (Some(Decision::Allow), true)
        );
        assert_eq!(
            decision_for(PromptOutcome::Allow { remember: false }),
            (Some(Decision::Allow), false)
        );
        assert_eq!(
            decision_for(PromptOutcome::Deny { remember: true }),
            (Some(Decision::Deny), true)
        );
        assert_eq!(decision_for(PromptOutcome::Defer), (None, false));
    }

    #[test]
    fn persist_decisions_writes_every_capability() {
        let store = Permissions::open_in_memory().unwrap();
        let caps = [Capability::Camera, Capability::Microphone];
        let (written, err) = persist_decisions(&store, "https://x", &caps, Decision::Allow);
        assert_eq!(written, 2);
        assert!(err.is_none());
        assert_eq!(
            store.get("https://x", Capability::Camera).unwrap(),
            Some(Decision::Allow)
        );
        assert_eq!(
            store.get("https://x", Capability::Microphone).unwrap(),
            Some(Decision::Allow)
        );
    }

    #[test]
    fn persist_decisions_empty_caps_is_noop() {
        let store = Permissions::open_in_memory().unwrap();
        let (written, err) = persist_decisions(&store, "https://x", &[], Decision::Deny);
        assert_eq!(written, 0);
        assert!(err.is_none());
    }

    // NOTE: the "store failed but the callback still fired" half of H9
    // cannot be exercised here — it needs both a live C++
    // `MediaAccessCallback` and a fault-injectable `Permissions` store,
    // and `Permissions` exposes no way to force a write failure. What IS
    // pinned above is the shape that makes the fix possible:
    // `persist_decisions` hands the error back instead of `?`-ing out of
    // `resolve` before the callback runs.
}
