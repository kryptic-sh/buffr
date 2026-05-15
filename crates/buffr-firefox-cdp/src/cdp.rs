//! CDP wire protocol types used by the Firefox CDP backend.
//!
//! Firefox supports a subset of Chrome DevTools Protocol via its Remote Agent.
//! This module covers only the methods confirmed working against Firefox as of
//! late 2025:
//!
//! Working:
//! - `Target.*` (limited): createTarget, closeTarget, getTargets, attachToTarget
//! - `Page.navigate`, `Page.reload`, `Page.stop`
//! - `Page.getNavigationHistory` + `Page.navigateToHistoryEntry`
//! - `Page.captureScreenshot` (used for OSR poll loop — Firefox has no screencast)
//! - `Runtime.evaluate`
//! - `Input.dispatchKeyEvent`, `Input.dispatchMouseEvent`
//! - `Page.frameNavigated` event
//! - `Emulation.setDeviceMetricsOverride`
//!
//! NOT supported in Firefox CDP (defer to Phase C / BiDi):
//! - `Page.startScreencast` / `Page.screencastFrame`
//! - `Browser.grantPermissions`
//! - `Page.handleJavaScriptDialog` (partial; deferred)

use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ── Message-id allocator ─────────────────────────────────────────────────────

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Allocate a monotonically increasing CDP message id.
pub fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

// ── Outgoing command ──────────────────────────────────────────────────────────

/// A CDP command sent over the WebSocket.
#[derive(Debug, Serialize)]
pub struct CdpCommand {
    pub id: u64,
    pub method: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    /// Present only for session-scoped commands (attached targets).
    #[serde(rename = "sessionId", skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

impl CdpCommand {
    pub fn new(method: &'static str, params: impl Serialize) -> Self {
        Self {
            id: next_id(),
            method,
            params: Some(serde_json::to_value(params).unwrap_or(Value::Null)),
            session_id: None,
        }
    }

    pub fn new_bare(method: &'static str) -> Self {
        Self {
            id: next_id(),
            method,
            params: None,
            session_id: None,
        }
    }

    pub fn with_session(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    pub fn serialize(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

// ── Incoming message ──────────────────────────────────────────────────────────

/// A CDP response or event received over the WebSocket.
#[derive(Debug, Deserialize)]
pub struct CdpMessage {
    /// Set on command responses; matches the outgoing `id`.
    pub id: Option<u64>,
    /// Set on events (e.g. `"Page.frameNavigated"`).
    pub method: Option<String>,
    /// Response result or event params.
    pub result: Option<Value>,
    /// CDP-level error for failed commands.
    pub error: Option<CdpError>,
    /// Session id for session-scoped events.
    #[serde(rename = "sessionId")]
    pub session_id: Option<String>,
    /// Event params.
    pub params: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub struct CdpError {
    pub code: i64,
    pub message: String,
}

// ── Specific command helpers ──────────────────────────────────────────────────

/// `Target.createTarget` params.
#[derive(Serialize)]
pub struct CreateTargetParams {
    pub url: String,
}

/// `Target.attachToTarget` params.
#[derive(Serialize)]
pub struct AttachToTargetParams {
    #[serde(rename = "targetId")]
    pub target_id: String,
    pub flatten: bool,
}

/// `Target.closeTarget` params.
#[derive(Serialize)]
pub struct CloseTargetParams {
    #[serde(rename = "targetId")]
    pub target_id: String,
}

/// `Page.navigate` params.
#[derive(Serialize)]
pub struct NavigateParams<'a> {
    pub url: &'a str,
}

/// `Page.captureScreenshot` params.
///
/// Firefox uses polling captureScreenshot instead of startScreencast because
/// Firefox CDP does not support `Page.startScreencast` as of late 2025.
/// This is the canonical Firefox-CDP OSR approach.
#[derive(Serialize)]
pub struct CaptureScreenshotParams {
    pub format: &'static str,
}

/// `Emulation.setDeviceMetricsOverride` params.
///
/// Firefox supports this via its CDP implementation; used to set the viewport.
#[derive(Serialize)]
pub struct SetDeviceMetricsParams {
    pub width: u32,
    pub height: u32,
    #[serde(rename = "deviceScaleFactor")]
    pub device_scale_factor: f64,
    pub mobile: bool,
}

/// `Input.dispatchMouseEvent` params.
#[derive(Serialize)]
pub struct DispatchMouseEventParams {
    #[serde(rename = "type")]
    pub event_type: &'static str,
    pub x: i32,
    pub y: i32,
    pub button: &'static str,
    #[serde(rename = "clickCount")]
    pub click_count: i32,
    pub modifiers: u32,
    #[serde(rename = "deltaX", skip_serializing_if = "Option::is_none")]
    pub delta_x: Option<f64>,
    #[serde(rename = "deltaY", skip_serializing_if = "Option::is_none")]
    pub delta_y: Option<f64>,
}

/// `Input.dispatchKeyEvent` params.
#[derive(Serialize)]
pub struct DispatchKeyEventParams {
    #[serde(rename = "type")]
    pub event_type: &'static str,
    #[serde(rename = "windowsVirtualKeyCode")]
    pub windows_virtual_key_code: i32,
    #[serde(rename = "nativeVirtualKeyCode")]
    pub native_virtual_key_code: i32,
    pub text: String,
    #[serde(rename = "unmodifiedText")]
    pub unmodified_text: String,
    pub modifiers: u32,
    #[serde(rename = "isSystemKey")]
    pub is_system_key: bool,
}

/// Map a [`buffr_engine::MouseButton`] to a CDP button string.
pub fn mouse_button_str(button: buffr_engine::MouseButton) -> &'static str {
    match button {
        buffr_engine::MouseButton::Left => "left",
        buffr_engine::MouseButton::Middle => "middle",
        buffr_engine::MouseButton::Right => "right",
        buffr_engine::MouseButton::Other(_) => "left",
    }
}

/// Map a [`buffr_engine::KeyEventKind`] to a CDP key event type string.
pub fn key_event_type(kind: buffr_engine::KeyEventKind) -> &'static str {
    match kind {
        buffr_engine::KeyEventKind::RawDown => "rawKeyDown",
        buffr_engine::KeyEventKind::Char => "char",
        buffr_engine::KeyEventKind::Up => "keyUp",
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_id_monotonic() {
        let a = next_id();
        let b = next_id();
        let c = next_id();
        assert!(b > a, "id should increase: {b} > {a}");
        assert!(c > b, "id should increase: {c} > {b}");
    }

    #[test]
    fn serialize_navigate_command() {
        let cmd = CdpCommand::new(
            "Page.navigate",
            NavigateParams {
                url: "https://example.com",
            },
        )
        .with_session("sess-1".to_owned());
        let json = cmd.serialize();
        let v: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(v["method"], "Page.navigate");
        assert_eq!(v["params"]["url"], "https://example.com");
        assert_eq!(v["sessionId"], "sess-1");
        assert!(v["id"].as_u64().unwrap_or(0) > 0);
    }

    #[test]
    fn parse_frame_navigated_event() {
        let json = r#"{
            "method": "Page.frameNavigated",
            "params": { "frame": { "url": "https://example.com" } },
            "sessionId": "s42"
        }"#;
        let msg: CdpMessage = serde_json::from_str(json).expect("parse");
        assert!(msg.id.is_none());
        assert_eq!(msg.method.as_deref(), Some("Page.frameNavigated"));
        assert_eq!(msg.session_id.as_deref(), Some("s42"));
        let frame_url = msg
            .params
            .as_ref()
            .and_then(|p| p.get("frame"))
            .and_then(|f| f.get("url"))
            .and_then(|u| u.as_str());
        assert_eq!(frame_url, Some("https://example.com"));
    }

    #[test]
    fn parse_error_response() {
        let json = r#"{"id": 5, "error": {"code": -32601, "message": "method not found"}}"#;
        let msg: CdpMessage = serde_json::from_str(json).expect("parse");
        assert_eq!(msg.id, Some(5));
        let err = msg.error.expect("error field present");
        assert_eq!(err.code, -32601);
        assert_eq!(err.message, "method not found");
    }

    #[test]
    fn serialize_capture_screenshot_params() {
        let params = CaptureScreenshotParams { format: "png" };
        let cmd = CdpCommand::new("Page.captureScreenshot", params);
        let v: serde_json::Value = serde_json::from_str(&cmd.serialize()).unwrap();
        assert_eq!(v["method"], "Page.captureScreenshot");
        assert_eq!(v["params"]["format"], "png");
    }

    #[test]
    fn serialize_set_device_metrics() {
        let params = SetDeviceMetricsParams {
            width: 1280,
            height: 800,
            device_scale_factor: 1.0,
            mobile: false,
        };
        let cmd = CdpCommand::new("Emulation.setDeviceMetricsOverride", params);
        let v: serde_json::Value = serde_json::from_str(&cmd.serialize()).unwrap();
        assert_eq!(v["method"], "Emulation.setDeviceMetricsOverride");
        assert_eq!(v["params"]["width"], 1280);
        assert_eq!(v["params"]["height"], 800);
    }

    #[test]
    fn mouse_button_str_mapping() {
        use buffr_engine::MouseButton;
        assert_eq!(mouse_button_str(MouseButton::Left), "left");
        assert_eq!(mouse_button_str(MouseButton::Middle), "middle");
        assert_eq!(mouse_button_str(MouseButton::Right), "right");
        assert_eq!(mouse_button_str(MouseButton::Other(7)), "left");
    }

    #[test]
    fn key_event_type_mapping() {
        use buffr_engine::KeyEventKind;
        assert_eq!(key_event_type(KeyEventKind::RawDown), "rawKeyDown");
        assert_eq!(key_event_type(KeyEventKind::Char), "char");
        assert_eq!(key_event_type(KeyEventKind::Up), "keyUp");
    }

    #[test]
    fn new_bare_command_has_no_params() {
        let cmd = CdpCommand::new_bare("Page.stop");
        let v: serde_json::Value = serde_json::from_str(&cmd.serialize()).unwrap();
        assert_eq!(v["method"], "Page.stop");
        assert!(v.get("params").is_none() || v["params"].is_null());
    }

    #[test]
    fn serialize_dispatch_mouse_event() {
        let params = DispatchMouseEventParams {
            event_type: "mousePressed",
            x: 100,
            y: 200,
            button: "left",
            click_count: 1,
            modifiers: 0,
            delta_x: None,
            delta_y: None,
        };
        let cmd =
            CdpCommand::new("Input.dispatchMouseEvent", params).with_session("sess".to_owned());
        let v: serde_json::Value = serde_json::from_str(&cmd.serialize()).unwrap();
        assert_eq!(v["params"]["button"], "left");
        assert_eq!(v["params"]["type"], "mousePressed");
        assert_eq!(v["params"]["x"], 100);
    }

    #[test]
    fn serialize_dispatch_key_event() {
        let params = DispatchKeyEventParams {
            event_type: "char",
            windows_virtual_key_code: 65,
            native_virtual_key_code: 65,
            text: "a".to_string(),
            unmodified_text: "a".to_string(),
            modifiers: 0,
            is_system_key: false,
        };
        let cmd = CdpCommand::new("Input.dispatchKeyEvent", params).with_session("sess".to_owned());
        let v: serde_json::Value = serde_json::from_str(&cmd.serialize()).unwrap();
        assert_eq!(v["params"]["text"], "a");
        assert_eq!(v["params"]["type"], "char");
    }
}
