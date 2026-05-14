//! Permission prompt wiring for the blink-cdp backend (Phase 8a, #88).
//!
//! # Approach
//!
//! Chromium 147's CDP `Browser` domain does not expose a stable
//! `Browser.permissionRequested` event. Instead, a small JS shim is
//! injected via `Page.addScriptToEvaluateOnNewDocument` that wraps the
//! four permission APIs:
//!
//! - `navigator.geolocation.getCurrentPosition` / `watchPosition`
//! - `Notification.requestPermission`
//! - `navigator.permissions.query`
//! - `navigator.mediaDevices.getUserMedia`
//!
//! When a page calls one of these, the shim:
//!
//! 1. Generates a unique request-id (`buffr-<n>`).
//! 2. Posts to the CDP binding `__buffrPermissionRequest` with a JSON
//!    payload `{ id, capability, origin }`.
//! 3. Returns a Promise that resolves when the worker calls
//!    `window.__buffrPermissionResolve(id, "granted"|"denied")` via
//!    `Runtime.evaluate`.
//!
//! The worker thread listens for `Runtime.bindingCalled` events and
//! pushes a neutral [`buffr_engine::permissions::PendingPermission`]
//! onto the shared [`buffr_engine::PermissionsQueue`].  When the UI
//! thread answers the prompt it calls
//! [`BlinkCdpEngine::resolve_permission`] which sends a
//! `Runtime.evaluate` to the correct session to resolve the in-page
//! Promise.
//!
//! # Capability string mapping
//!
//! | JS source                           | capability string  | Capability          |
//! |-------------------------------------|--------------------|---------------------|
//! | `geolocation.getCurrentPosition`    | `"geolocation"`    | `Geolocation`       |
//! | `geolocation.watchPosition`         | `"geolocation"`    | `Geolocation`       |
//! | `Notification.requestPermission`    | `"notifications"`  | `Notifications`     |
//! | `permissions.query({name:"…"})`     | query name         | (name-mapped)       |
//! | `mediaDevices.getUserMedia(video:…)`| `"camera"`         | `Camera`            |
//! | `mediaDevices.getUserMedia(audio:…)`| `"microphone"`     | `Microphone`        |
//!
//! # Deferred items
//!
//! - Auto-grant by URL match: future work.
//! - Persistent permission storage across sessions: deferred to a
//!   `buffr-permissions` extension; Phase 8a is in-memory only.
//! - `Browser.permissionRequested` event-based approach: investigate
//!   in a future patch if more reliable than the shim.

use buffr_permissions::Capability;

// ── Capability mapping ────────────────────────────────────────────────────────

/// Convert a capability name string (from the JS shim) to a
/// [`Capability`] value. Returns `None` for unrecognised names —
/// callers should fall through to `Capability::Other(0)` or skip.
pub fn capability_from_str(name: &str) -> Option<Capability> {
    match name {
        "geolocation" => Some(Capability::Geolocation),
        "notifications" | "push" => Some(Capability::Notifications),
        "camera" => Some(Capability::Camera),
        "microphone" => Some(Capability::Microphone),
        "clipboard-read" | "clipboard-write" | "clipboard" => Some(Capability::Clipboard),
        "midi" | "midi-sysex" => Some(Capability::Midi),
        _ => None,
    }
}

// ── JS shim ───────────────────────────────────────────────────────────────────

/// Generate the JS shim source injected via
/// `Page.addScriptToEvaluateOnNewDocument`.
///
/// The shim replaces the four permission-request APIs with wrappers that
/// post a `__buffrPermissionRequest` binding call and return a Promise
/// that resolves when `window.__buffrPermissionResolve(id, outcome)` is
/// called by the worker thread.
///
/// The shim is intentionally minimal — no dependencies on external
/// libraries, no `eval`, no dynamic code generation beyond the binding
/// name which is a compile-time constant.
pub fn permission_shim_js() -> String {
    r#"
(function () {
  'use strict';

  // Map from request-id → { resolve, reject } for pending permission
  // Promises. Keyed by a monotonic counter prefixed "buffr-".
  const _pendingPerms = {};
  let _permCounter = 0;

  // Called by the Rust worker via Runtime.evaluate to resolve a
  // pending Promise.
  window.__buffrPermissionResolve = function (id, outcome) {
    const entry = _pendingPerms[id];
    if (!entry) return;
    delete _pendingPerms[id];
    entry.resolve(outcome === 'granted' ? 'granted' : 'denied');
  };

  // Post a binding call to the Rust worker and return a Promise.
  function _requestPerm(capability, origin) {
    const id = 'buffr-' + (++_permCounter);
    return new Promise(function (resolve, reject) {
      _pendingPerms[id] = { resolve, reject };
      try {
        window.__buffrPermissionRequest(JSON.stringify({
          id: id,
          capability: capability,
          origin: origin || (window.location && window.location.origin) || ''
        }));
      } catch (e) {
        // If the binding isn't registered yet, deny immediately so
        // the page doesn't hang.
        delete _pendingPerms[id];
        reject(e);
      }
    });
  }

  // ── Geolocation ─────────────────────────────────────────────────────────────
  if (navigator.geolocation) {
    const _origGeo = navigator.geolocation;
    const _origGetCurrent = _origGeo.getCurrentPosition.bind(_origGeo);
    const _origWatch = _origGeo.watchPosition.bind(_origGeo);

    navigator.geolocation.getCurrentPosition = function (success, error, opts) {
      _requestPerm('geolocation', '').then(function (outcome) {
        if (outcome === 'granted') {
          _origGetCurrent(success, error, opts);
        } else if (error) {
          error({ code: 1, message: 'Permission denied by buffr' });
        }
      });
    };

    navigator.geolocation.watchPosition = function (success, error, opts) {
      let watchId = -1;
      _requestPerm('geolocation', '').then(function (outcome) {
        if (outcome === 'granted') {
          watchId = _origWatch(success, error, opts);
        } else if (error) {
          error({ code: 1, message: 'Permission denied by buffr' });
        }
      });
      return watchId;
    };
  }

  // ── Notifications ────────────────────────────────────────────────────────────
  if (window.Notification) {
    const _origNotifReq = Notification.requestPermission.bind(Notification);
    Notification.requestPermission = function (callback) {
      const p = _requestPerm('notifications', '').then(function (outcome) {
        if (callback) callback(outcome);
        return outcome;
      });
      return p;
    };
  }

  // ── navigator.permissions.query ──────────────────────────────────────────────
  if (navigator.permissions) {
    const _origQuery = navigator.permissions.query.bind(navigator.permissions);
    navigator.permissions.query = function (desc) {
      const name = desc && desc.name ? desc.name : '';
      return _requestPerm(name, '').then(function (outcome) {
        return { state: outcome === 'granted' ? 'granted' : 'denied', name: name };
      });
    };
  }

  // ── mediaDevices.getUserMedia ─────────────────────────────────────────────────
  if (navigator.mediaDevices && navigator.mediaDevices.getUserMedia) {
    const _origGUM = navigator.mediaDevices.getUserMedia.bind(navigator.mediaDevices);
    navigator.mediaDevices.getUserMedia = function (constraints) {
      const caps = [];
      if (constraints) {
        if (constraints.video) caps.push('camera');
        if (constraints.audio) caps.push('microphone');
      }
      const cap = caps.length === 1 ? caps[0] : (caps.length > 1 ? 'camera' : 'camera');
      return _requestPerm(cap, '').then(function (outcome) {
        if (outcome === 'granted') {
          return _origGUM(constraints);
        }
        return Promise.reject(new DOMException('Permission denied', 'NotAllowedError'));
      });
    };
  }
})();
"#
    .to_string()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use buffr_permissions::Capability;

    // ── capability_from_str tests ─────────────────────────────────────────────

    #[test]
    fn capability_from_str_geolocation() {
        assert_eq!(
            capability_from_str("geolocation"),
            Some(Capability::Geolocation)
        );
    }

    #[test]
    fn capability_from_str_notifications() {
        assert_eq!(
            capability_from_str("notifications"),
            Some(Capability::Notifications)
        );
        assert_eq!(capability_from_str("push"), Some(Capability::Notifications));
    }

    #[test]
    fn capability_from_str_camera() {
        assert_eq!(capability_from_str("camera"), Some(Capability::Camera));
    }

    #[test]
    fn capability_from_str_microphone() {
        assert_eq!(
            capability_from_str("microphone"),
            Some(Capability::Microphone)
        );
    }

    #[test]
    fn capability_from_str_clipboard() {
        assert_eq!(
            capability_from_str("clipboard-read"),
            Some(Capability::Clipboard)
        );
        assert_eq!(
            capability_from_str("clipboard-write"),
            Some(Capability::Clipboard)
        );
        assert_eq!(
            capability_from_str("clipboard"),
            Some(Capability::Clipboard)
        );
    }

    #[test]
    fn capability_from_str_midi() {
        assert_eq!(capability_from_str("midi"), Some(Capability::Midi));
        assert_eq!(capability_from_str("midi-sysex"), Some(Capability::Midi));
    }

    #[test]
    fn capability_from_str_unknown_returns_none() {
        assert!(capability_from_str("storage-access").is_none());
        assert!(capability_from_str("").is_none());
        assert!(capability_from_str("payment-handler").is_none());
    }

    // ── shim JS generation tests ──────────────────────────────────────────────

    #[test]
    fn shim_js_contains_binding_name() {
        let js = permission_shim_js();
        assert!(
            js.contains("__buffrPermissionRequest"),
            "shim must reference the CDP binding name"
        );
    }

    #[test]
    fn shim_js_contains_resolve_fn() {
        let js = permission_shim_js();
        assert!(
            js.contains("__buffrPermissionResolve"),
            "shim must define the resolve function"
        );
    }

    #[test]
    fn shim_js_covers_geolocation() {
        let js = permission_shim_js();
        assert!(
            js.contains("navigator.geolocation"),
            "shim must wrap geolocation"
        );
        assert!(
            js.contains("getCurrentPosition"),
            "shim must wrap getCurrentPosition"
        );
        assert!(js.contains("watchPosition"), "shim must wrap watchPosition");
    }

    #[test]
    fn shim_js_covers_notifications() {
        let js = permission_shim_js();
        assert!(
            js.contains("Notification.requestPermission"),
            "shim must wrap Notification.requestPermission"
        );
    }

    #[test]
    fn shim_js_covers_permissions_query() {
        let js = permission_shim_js();
        assert!(
            js.contains("navigator.permissions.query"),
            "shim must wrap navigator.permissions.query"
        );
    }

    #[test]
    fn shim_js_covers_get_user_media() {
        let js = permission_shim_js();
        assert!(js.contains("getUserMedia"), "shim must wrap getUserMedia");
    }

    #[test]
    fn shim_js_not_empty() {
        let js = permission_shim_js();
        assert!(!js.trim().is_empty(), "shim JS must not be empty");
    }

    // ── bindingCalled payload → PendingPermission mapping test ───────────────

    #[test]
    fn binding_payload_parse_geolocation() {
        // Simulate the JSON payload posted by the shim.
        let payload =
            r#"{"id":"buffr-1","capability":"geolocation","origin":"https://example.com"}"#;
        let v: serde_json::Value = serde_json::from_str(payload).unwrap();
        let id = v["id"].as_str().unwrap();
        let cap_str = v["capability"].as_str().unwrap();
        let origin = v["origin"].as_str().unwrap();

        assert_eq!(id, "buffr-1");
        assert_eq!(origin, "https://example.com");
        let cap = capability_from_str(cap_str).unwrap();
        assert_eq!(cap, Capability::Geolocation);
    }

    #[test]
    fn binding_payload_parse_camera() {
        let payload =
            r#"{"id":"buffr-2","capability":"camera","origin":"https://meet.example.com"}"#;
        let v: serde_json::Value = serde_json::from_str(payload).unwrap();
        let cap = capability_from_str(v["capability"].as_str().unwrap()).unwrap();
        assert_eq!(cap, Capability::Camera);
    }

    #[test]
    fn binding_payload_parse_unknown_capability() {
        let payload = r#"{"id":"buffr-3","capability":"payment-handler","origin":"https://shop.example.com"}"#;
        let v: serde_json::Value = serde_json::from_str(payload).unwrap();
        let cap = capability_from_str(v["capability"].as_str().unwrap());
        assert!(cap.is_none(), "unknown capability should return None");
    }
}
