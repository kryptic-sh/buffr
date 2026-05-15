//! Neutral input event → Ladybird FFI event translation.
//!
//! Ladybird's `KeyCode` enum (LibWeb/UIEvents/KeyCode.h) uses Windows VK_*
//! values for most keys — the same table CEF and buffr-engine's `NeutralKeyEvent`
//! already use.  We therefore pass `windows_key_code` through unchanged.
//!
//! Mouse buttons: Ladybird uses 0=Left, 1=Middle, 2=Right — matching the DOM
//! `MouseEvent.button` property.  `buffr-engine::MouseButton` maps cleanly.
//!
//! # Phase C notes
//!
//! - Modifier bitmasks: CEF EVENTFLAG_* (Ctrl=2, Shift=4, Alt=8) differ from
//!   Ladybird's `KeyModifier` flags (Ctrl=0x01, Shift=0x02, Alt=0x04).
//!   Phase C must add a `translate_modifiers` conversion here.
//! - `KeyEventKind` → Ladybird distinguishes `KeyDown` / `KeyUp` but does not
//!   have a separate `Char` event; inject `text` on `KeyDown` instead.

use buffr_engine::{MouseButton, NeutralKeyEvent, input::KeyEventKind};

/// A translated Ladybird key event (lightweight struct for Phase B logging).
pub(crate) struct LadybirdKeyEvent {
    pub key_code: u32,
    pub is_press: bool,
    pub modifiers: u32,
}

/// A translated Ladybird mouse event.
pub(crate) struct LadybirdMouseEvent {
    pub x: i32,
    pub y: i32,
    pub kind: LadybirdMouseKind,
}

pub(crate) enum LadybirdMouseKind {
    Move,
    Button { button: u32, is_press: bool },
    Scroll { dx: f64, dy: f64 },
    Leave,
}

// ── Translators ───────────────────────────────────────────────────────────────

pub(crate) fn neutral_key_to_ladybird(event: &NeutralKeyEvent) -> LadybirdKeyEvent {
    let is_press = !matches!(event.kind, KeyEventKind::Up);
    // TODO(phase-c): map CEF EVENTFLAG_* modifiers to Ladybird KeyModifier flags.
    // For Phase B we pass the raw bitmask through — it is only logged, not dispatched.
    LadybirdKeyEvent {
        key_code: event.windows_key_code as u32,
        is_press,
        modifiers: event.modifiers,
    }
}

/// `MouseButton` → DOM `MouseEvent.button` index used by Ladybird.
pub(crate) fn mouse_button_index(btn: MouseButton) -> u32 {
    match btn {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
        // Extended buttons: Ladybird does not expose them yet; fall back to Left.
        MouseButton::Other(_) => 0,
    }
}

pub(crate) fn neutral_move_to_ladybird(x: i32, y: i32) -> LadybirdMouseEvent {
    LadybirdMouseEvent {
        x,
        y,
        kind: LadybirdMouseKind::Move,
    }
}

pub(crate) fn neutral_click_to_ladybird(
    x: i32,
    y: i32,
    button: MouseButton,
    mouse_up: bool,
) -> LadybirdMouseEvent {
    LadybirdMouseEvent {
        x,
        y,
        kind: LadybirdMouseKind::Button {
            button: mouse_button_index(button),
            is_press: !mouse_up,
        },
    }
}

pub(crate) fn neutral_leave_to_ladybird() -> LadybirdMouseEvent {
    LadybirdMouseEvent {
        x: 0,
        y: 0,
        kind: LadybirdMouseKind::Leave,
    }
}

pub(crate) fn neutral_scroll_to_ladybird(
    x: i32,
    y: i32,
    delta_x: i32,
    delta_y: i32,
) -> LadybirdMouseEvent {
    LadybirdMouseEvent {
        x,
        y,
        kind: LadybirdMouseKind::Scroll {
            dx: delta_x as f64,
            dy: delta_y as f64,
        },
    }
}
