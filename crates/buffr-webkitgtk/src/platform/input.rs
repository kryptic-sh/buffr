//! Neutral input event translation for the WebKitGTK backend.
//!
//! # Phase C status
//!
//! GTK4 / gdk4 0.11 does not expose safe Rust constructors for synthetic
//! `GdkEvent`. Instead, Phase C dispatches input via JS injection through
//! `WebView::evaluate_javascript`. See `input_js.rs` for the snippet builders.
//!
//! # Phase D note
//!
//! Once gdk4 >= 0.12 exposes synthetic event constructors, replace the JS
//! injection path with `WebView::event()` for full native-action coverage
//! (Tab focus traversal, form submit, etc.).

use buffr_engine::{MouseButton, NeutralKeyEvent};

/// Translated key event.
///
/// Carries the Windows VK code (for JS key mapping) and a human-readable
/// description (for debug logging when the VK is unmapped).
pub(crate) struct GtkKeyEvent {
    /// Windows VK_* code used for JS KeyboardEvent dispatch.
    pub windows_key_code: i32,
    /// Human-readable description for debug logging.
    pub description: String,
}

/// Translated mouse event — Phase B placeholder.
pub(crate) struct GtkMouseEvent {
    pub x: f64,
    pub y: f64,
    #[allow(dead_code)]
    pub kind: GtkMouseKind,
}

pub(crate) enum GtkMouseKind {
    Move,
    ButtonPress(GtkMouseButton),
    ButtonRelease(GtkMouseButton),
    Leave,
    /// Wheel scroll stored separately as `GtkWheelEvent`; this variant is
    /// retained for structural symmetry but the worker matches on `Wheel`.
    #[allow(dead_code)]
    Scroll {
        delta_x: f64,
        delta_y: f64,
    },
}

pub(crate) enum GtkMouseButton {
    Left,
    Middle,
    Right,
}

/// Translated wheel event carrying position and pixel deltas.
pub(crate) struct GtkWheelEvent {
    pub x: f64,
    pub y: f64,
    pub delta_x: f64,
    pub delta_y: f64,
}

// ── Neutral wrapper sent over the worker command channel ──────────────────────

/// A translated GTK input event ready to be routed to the worker.
///
/// The worker receives this on the GTK main thread. Phase C dispatches via
/// JS injection through `WebView::evaluate_javascript`. Phase D may switch
/// to native `gdk4::Event` dispatch once safe constructors are available.
pub(crate) enum GtkInputEvent {
    Key(GtkKeyEvent),
    Mouse(GtkMouseEvent),
    Wheel(GtkWheelEvent),
    /// IME composition / commit / cancel dispatched via JS CompositionEvent.
    Ime(GtkImeEvent),
}

/// IME event payload for the JS-injection IME path.
pub(crate) enum GtkImeEvent {
    /// Preedit update: live composition string + optional (start, end) cursor.
    Preedit {
        text: String,
        cursor: Option<(usize, usize)>,
    },
    /// Commit: accepted composition text inserted into the focused element.
    Commit { text: String },
    /// Cancel: composition was dismissed with no committed text.
    Cancel,
}

// ── Translators ───────────────────────────────────────────────────────────────

pub(crate) fn neutral_key_to_gtk(event: &NeutralKeyEvent) -> GtkInputEvent {
    GtkInputEvent::Key(GtkKeyEvent {
        windows_key_code: event.windows_key_code,
        description: format!(
            "key vk={} char={} kind={:?}",
            event.windows_key_code, event.character, event.kind
        ),
    })
}

pub(crate) fn neutral_move_to_gtk(x: i32, y: i32) -> GtkInputEvent {
    GtkInputEvent::Mouse(GtkMouseEvent {
        x: x as f64,
        y: y as f64,
        kind: GtkMouseKind::Move,
    })
}

pub(crate) fn neutral_click_to_gtk(
    x: i32,
    y: i32,
    button: MouseButton,
    mouse_up: bool,
) -> GtkInputEvent {
    let btn = match button {
        MouseButton::Left => GtkMouseButton::Left,
        MouseButton::Middle => GtkMouseButton::Middle,
        MouseButton::Right | MouseButton::Other(_) => GtkMouseButton::Right,
    };
    GtkInputEvent::Mouse(GtkMouseEvent {
        x: x as f64,
        y: y as f64,
        kind: if mouse_up {
            GtkMouseKind::ButtonRelease(btn)
        } else {
            GtkMouseKind::ButtonPress(btn)
        },
    })
}

pub(crate) fn neutral_leave_to_gtk() -> GtkInputEvent {
    GtkInputEvent::Mouse(GtkMouseEvent {
        x: 0.0,
        y: 0.0,
        kind: GtkMouseKind::Leave,
    })
}

pub(crate) fn neutral_scroll_to_gtk(x: i32, y: i32, dx: i32, dy: i32) -> GtkInputEvent {
    GtkInputEvent::Wheel(GtkWheelEvent {
        x: x as f64,
        y: y as f64,
        delta_x: dx as f64,
        delta_y: dy as f64,
    })
}
