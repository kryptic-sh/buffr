//! Neutral input event translation for the WebKitGTK backend.
//!
//! # Phase B status
//!
//! WebKitGTK / GTK4 input synthesis requires building `gdk4::Event` objects
//! and dispatching them via `WebView::send_input_event()` or equivalent.
//! The exact API depends on whether the GTK4 `gdk4` Rust crate exposes a
//! public constructor for synthetic events.
//!
//! In `gtk4 0.11` / `gdk4 0.11`, synthetic event construction is not directly
//! exposed in safe Rust. Dispatch via `WebView::event()` (which takes a
//! `&gdk4::Event`) requires a fully constructed `GdkEvent` which cannot be
//! built without unsafe FFI at this crate version.
//!
//! Phase B: log + no-op for all input synthesis. The debug log records the
//! event kind so tests can confirm routing reaches here.
//!
//! # TODO markers
//!
//! - TODO(input-key): wire `gdk4` synthetic key event when safe constructors
//!   are available in gdk4 >= 0.12 or via unsafe FFI.
//! - TODO(input-mouse): wire `gdk4` synthetic pointer/scroll events.

use buffr_engine::{MouseButton, NeutralKeyEvent};

/// Translated key event — Phase B placeholder.
pub(crate) struct GtkKeyEvent {
    pub description: String,
}

/// Translated mouse event — Phase B placeholder.
pub(crate) struct GtkMouseEvent {
    pub x: f64,
    pub y: f64,
    #[allow(dead_code)]
    pub kind: GtkMouseKind,
}

#[allow(dead_code)]
pub(crate) enum GtkMouseKind {
    Move,
    ButtonPress(GtkMouseButton),
    ButtonRelease(GtkMouseButton),
    Leave,
    Scroll { delta_x: f64, delta_y: f64 },
}

#[allow(dead_code)]
pub(crate) enum GtkMouseButton {
    Left,
    Middle,
    Right,
}

// ── Translators ───────────────────────────────────────────────────────────────

pub(crate) fn neutral_key_to_gtk(event: &NeutralKeyEvent) -> GtkKeyEvent {
    GtkKeyEvent {
        description: format!(
            "key vk={} char={} kind={:?}",
            event.windows_key_code, event.character, event.kind
        ),
    }
}

pub(crate) fn neutral_move_to_gtk(x: i32, y: i32) -> GtkMouseEvent {
    GtkMouseEvent {
        x: x as f64,
        y: y as f64,
        kind: GtkMouseKind::Move,
    }
}

pub(crate) fn neutral_click_to_gtk(
    x: i32,
    y: i32,
    button: MouseButton,
    mouse_up: bool,
) -> GtkMouseEvent {
    let btn = match button {
        MouseButton::Left => GtkMouseButton::Left,
        MouseButton::Middle => GtkMouseButton::Middle,
        MouseButton::Right | MouseButton::Other(_) => GtkMouseButton::Right,
    };
    GtkMouseEvent {
        x: x as f64,
        y: y as f64,
        kind: if mouse_up {
            GtkMouseKind::ButtonRelease(btn)
        } else {
            GtkMouseKind::ButtonPress(btn)
        },
    }
}

pub(crate) fn neutral_leave_to_gtk() -> GtkMouseEvent {
    GtkMouseEvent {
        x: 0.0,
        y: 0.0,
        kind: GtkMouseKind::Leave,
    }
}

pub(crate) fn neutral_scroll_to_gtk(x: i32, y: i32, dx: i32, dy: i32) -> GtkMouseEvent {
    GtkMouseEvent {
        x: x as f64,
        y: y as f64,
        kind: GtkMouseKind::Scroll {
            delta_x: dx as f64,
            delta_y: dy as f64,
        },
    }
}
