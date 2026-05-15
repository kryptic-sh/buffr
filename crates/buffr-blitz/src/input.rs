//! Neutral input event → Blitz event translation.
//!
//! Blitz (via blitz-traits) uses `keyboard_types::KeyboardEvent` for key
//! events and `blitz_traits::events` for mouse events.  We build lightweight
//! wrappers from the neutral `buffr-engine` input types.
//!
//! Note: `HtmlDocument` / `BaseDocument` does not yet expose a single
//! `dispatch_event()` method in the 0.3.0-alpha.2 API.  The translation
//! functions are defined here for Phase C wiring; Phase B logs the events.

use buffr_engine::{MouseButton, NeutralKeyEvent};

/// A translated key event (Blitz representation placeholder).
///
/// Phase B: stores the description for debug logging.
/// Phase C: replace with `keyboard_types::KeyboardEvent` when the
/// `BaseDocument::dispatch_event` API stabilises.
pub(crate) struct BlitzKeyEvent {
    pub description: String,
}

/// A translated mouse event.
pub(crate) struct BlitzMouseEvent {
    pub x: f32,
    pub y: f32,
    /// Kept for Phase C event dispatch to Blitz; logged in Phase B.
    #[allow(dead_code)]
    pub kind: BlitzMouseKind,
}

#[allow(dead_code)]
pub(crate) enum BlitzMouseKind {
    Move,
    Down(BlitzMouseButton),
    Up(BlitzMouseButton),
    Leave,
    Wheel { delta_x: f32, delta_y: f32 },
}

#[allow(dead_code)]
pub(crate) enum BlitzMouseButton {
    Left,
    Middle,
    Right,
}

// ── Translators ───────────────────────────────────────────────────────────────

pub(crate) fn neutral_key_to_blitz(event: &NeutralKeyEvent) -> BlitzKeyEvent {
    let desc = format!(
        "key vk={} char={} kind={:?}",
        event.windows_key_code, event.character, event.kind
    );
    BlitzKeyEvent { description: desc }
}

pub(crate) fn neutral_move_to_blitz(x: i32, y: i32) -> BlitzMouseEvent {
    BlitzMouseEvent {
        x: x as f32,
        y: y as f32,
        kind: BlitzMouseKind::Move,
    }
}

pub(crate) fn neutral_click_to_blitz(
    x: i32,
    y: i32,
    button: MouseButton,
    mouse_up: bool,
) -> BlitzMouseEvent {
    let btn = match button {
        MouseButton::Left => BlitzMouseButton::Left,
        MouseButton::Middle => BlitzMouseButton::Middle,
        MouseButton::Right | MouseButton::Other(_) => BlitzMouseButton::Right,
    };
    BlitzMouseEvent {
        x: x as f32,
        y: y as f32,
        kind: if mouse_up {
            BlitzMouseKind::Up(btn)
        } else {
            BlitzMouseKind::Down(btn)
        },
    }
}

pub(crate) fn neutral_leave_to_blitz() -> BlitzMouseEvent {
    BlitzMouseEvent {
        x: 0.0,
        y: 0.0,
        kind: BlitzMouseKind::Leave,
    }
}

pub(crate) fn neutral_scroll_to_blitz(
    x: i32,
    y: i32,
    delta_x: i32,
    delta_y: i32,
) -> BlitzMouseEvent {
    BlitzMouseEvent {
        x: x as f32,
        y: y as f32,
        kind: BlitzMouseKind::Wheel {
            delta_x: delta_x as f32,
            delta_y: delta_y as f32,
        },
    }
}
