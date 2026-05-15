//! Input event translation: buffr-engine neutral types → servo InputEvent.
//!
//! This module is shape-complete for Phase B.  The actual Servo type imports
//! are blocked by a dep conflict (servo 0.1 → libsqlite3-sys 0.35.x vs.
//! workspace → libsqlite3-sys 0.37.x).  See `Cargo.toml` for the full
//! conflict description.
//!
//! When the dep conflict is resolved, restore these imports and remove the
//! placeholder types below:
//!
//! ```rust,ignore
//! // TODO: restore when servo dep conflict is fixed
//! use keyboard_types::{Code, Key, KeyState, KeyboardEvent, Location, Modifiers};
//! use servo::input_events::{
//!     InputEvent, MouseButton as ServoMouseButton, MouseButtonAction, MouseButtonEvent,
//!     MouseLeftViewportEvent, MouseMoveEvent, WheelDelta, WheelEvent, WheelMode,
//! };
//! use servo::webview::WebViewPoint;
//! ```

use buffr_engine::{KeyEventKind, MouseButton as NeutralButton, NeutralKeyEvent};

// ── Placeholder input event type (shape-complete skeleton) ────────────────────

/// Opaque placeholder for `servo::input_events::InputEvent`.
///
/// Replace with the real type when the libsqlite3-sys dep conflict is resolved.
#[derive(Debug, Clone)]
pub struct ServoInputEvent {
    /// Description for logging; serialised by the worker.
    pub description: String,
}

// ── Modifier conversion ───────────────────────────────────────────────────────

/// Convert CEF EVENTFLAG_* bitmask to a human-readable description (placeholder).
///
/// Real version returns `keyboard_types::Modifiers`.
fn describe_mods(mods: u32) -> String {
    let mut parts = Vec::new();
    if mods & 2 != 0 {
        parts.push("Shift");
    }
    if mods & 4 != 0 {
        parts.push("Ctrl");
    }
    if mods & 8 != 0 {
        parts.push("Alt");
    }
    if mods & 64 != 0 {
        parts.push("Meta");
    }
    if parts.is_empty() {
        String::new()
    } else {
        parts.join("+")
    }
}

// ── Windows VK → name ─────────────────────────────────────────────────────────

/// Map a Windows VK_* code to a debug name (placeholder).
///
/// Real version returns `keyboard_types::Key`.
fn vk_name(vk: i32, character: u16) -> String {
    if character != 0
        && let Some(c) = char::from_u32(character as u32)
    {
        return format!("Char({c})");
    }
    match vk {
        0x08 => "Backspace".into(),
        0x09 => "Tab".into(),
        0x0D => "Enter".into(),
        0x1B => "Escape".into(),
        0x20 => "Space".into(),
        0x21 => "PageUp".into(),
        0x22 => "PageDown".into(),
        0x23 => "End".into(),
        0x24 => "Home".into(),
        0x25 => "ArrowLeft".into(),
        0x26 => "ArrowUp".into(),
        0x27 => "ArrowRight".into(),
        0x28 => "ArrowDown".into(),
        0x2E => "Delete".into(),
        0x10 => "Shift".into(),
        0x11 => "Control".into(),
        0x12 => "Alt".into(),
        0x70 => "F1".into(),
        0x71 => "F2".into(),
        0x72 => "F3".into(),
        0x73 => "F4".into(),
        0x74 => "F5".into(),
        0x75 => "F6".into(),
        0x76 => "F7".into(),
        0x77 => "F8".into(),
        0x78 => "F9".into(),
        0x79 => "F10".into(),
        0x7A => "F11".into(),
        0x7B => "F12".into(),
        v => format!("VK(0x{v:02X})"),
    }
}

// ── Public helpers ────────────────────────────────────────────────────────────

/// Translate a [`NeutralKeyEvent`] to a placeholder [`ServoInputEvent`].
///
/// TODO: when dep conflict is resolved, return `servo::input_events::InputEvent`.
pub fn neutral_key_to_servo(ev: &NeutralKeyEvent) -> ServoInputEvent {
    let state = match ev.kind {
        KeyEventKind::Up => "keyup",
        KeyEventKind::RawDown | KeyEventKind::Char => "keydown",
    };
    let key = vk_name(ev.windows_key_code, ev.character);
    let mods = describe_mods(ev.modifiers);
    tracing::debug!("servo input: {state} key={key} mods={mods}");
    ServoInputEvent {
        description: format!("{state}:{key}:{mods}"),
    }
}

/// Translate a neutral mouse-click to a placeholder [`ServoInputEvent`].
pub fn neutral_click_to_servo(
    x: i32,
    y: i32,
    button: NeutralButton,
    mouse_up: bool,
) -> ServoInputEvent {
    let btn = match button {
        NeutralButton::Left => "Left",
        NeutralButton::Middle => "Middle",
        NeutralButton::Right => "Right",
        NeutralButton::Other(_) => "Other",
    };
    let action = if mouse_up { "mouseup" } else { "mousedown" };
    ServoInputEvent {
        description: format!("{action}:{btn}@{x},{y}"),
    }
}

/// Translate a neutral mouse-move to a placeholder [`ServoInputEvent`].
pub fn neutral_move_to_servo(x: i32, y: i32) -> ServoInputEvent {
    ServoInputEvent {
        description: format!("mousemove@{x},{y}"),
    }
}

/// Translate a neutral mouse-leave to a placeholder [`ServoInputEvent`].
pub fn neutral_leave_to_servo() -> ServoInputEvent {
    ServoInputEvent {
        description: "mouseleave".into(),
    }
}

/// Translate a neutral scroll to a placeholder [`ServoInputEvent`].
pub fn neutral_scroll_to_servo(x: i32, y: i32, delta_x: i32, delta_y: i32) -> ServoInputEvent {
    ServoInputEvent {
        description: format!("wheel@{x},{y} delta={delta_x},{delta_y}"),
    }
}
