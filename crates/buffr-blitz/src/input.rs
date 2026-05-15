//! Neutral input event → Blitz event translation.
//!
//! `blitz-dom 0.3.0-alpha.2` exposes `Document::handle_ui_event(UiEvent)` for
//! dispatching pointer and keyboard events from outside the document.
//! `HtmlDocument` implements `Document` (via `DerefMut` to `BaseDocument`), so
//! `doc.handle_ui_event(ev)` is the real dispatch entry point.
//!
//! We translate the neutral `buffr-engine` input types to `blitz_traits::events`
//! types here, then wrap them in the `BlitzInputEvent` enum so the worker can
//! own the translated event and call `handle_ui_event` on the right thread.

use blitz_traits::events::{
    BlitzKeyEvent as BKeyEvent, BlitzPointerEvent, BlitzPointerId, BlitzWheelDelta,
    BlitzWheelEvent, KeyState, MouseEventButton, MouseEventButtons, PointerCoords, PointerDetails,
};
use keyboard_types::{Code, Key, Location, Modifiers};

use buffr_engine::{KeyEventKind, MouseButton, NeutralKeyEvent};

// ── Neutral BlitzInputEvent (owned, Send, single-enum wrapper for worker) ─────

/// Translated Blitz input event — owned by the worker command.
///
/// The worker calls `tab.doc.handle_ui_event(ev.into_ui_event())` on the
/// active `BlitzTab`.
pub(crate) enum BlitzInputEvent {
    Key(BKeyEvent),
    PointerMove(BlitzPointerEvent),
    PointerDown(BlitzPointerEvent),
    PointerUp(BlitzPointerEvent),
    PointerLeave,
    Wheel(BlitzWheelEvent),
}

impl BlitzInputEvent {
    /// Convert into a `blitz_traits::events::UiEvent` for dispatch.
    pub(crate) fn into_ui_event(self) -> blitz_traits::events::UiEvent {
        use blitz_traits::events::UiEvent;
        match self {
            BlitzInputEvent::Key(ev) => {
                if ev.state == KeyState::Pressed {
                    UiEvent::KeyDown(ev)
                } else {
                    UiEvent::KeyUp(ev)
                }
            }
            BlitzInputEvent::PointerMove(ev) => UiEvent::PointerMove(ev),
            BlitzInputEvent::PointerDown(ev) => UiEvent::PointerDown(ev),
            BlitzInputEvent::PointerUp(ev) => UiEvent::PointerUp(ev),
            BlitzInputEvent::PointerLeave => {
                // Synthesise a pointer-move to (0,0) with no buttons pressed.
                // There is no explicit Leave variant in UiEvent; the hover
                // state is cleared when nothing hits at the new coordinates.
                UiEvent::PointerMove(make_pointer_event(
                    0.0,
                    0.0,
                    MouseEventButton::Main,
                    MouseEventButtons::None,
                ))
            }
            BlitzInputEvent::Wheel(ev) => UiEvent::Wheel(ev),
        }
    }
}

// ── Translators ───────────────────────────────────────────────────────────────

/// Translate a neutral key event to a `BlitzInputEvent::Key`.
pub(crate) fn neutral_key_to_blitz(event: &NeutralKeyEvent) -> BlitzInputEvent {
    // NeutralKeyEvent::character is a UTF-16 code unit (u16).  Decode it to a
    // Rust char so we can produce a keyboard_types::Key::Character value.
    let ch: Option<char> = char::from_u32(event.character as u32).filter(|&c| c != '\0');
    let key = match ch {
        Some(c) => Key::Character(c.to_string()),
        None => Key::Unidentified,
    };
    let state = match event.kind {
        KeyEventKind::Up => KeyState::Released,
        _ => KeyState::Pressed,
    };
    BlitzInputEvent::Key(BKeyEvent {
        key,
        code: Code::Unidentified,
        modifiers: Modifiers::empty(),
        location: Location::Standard,
        is_auto_repeating: matches!(event.kind, KeyEventKind::RawDown),
        is_composing: false,
        state,
        text: if let (Some(c), KeyState::Pressed) = (ch, state) {
            Some(smol_str::SmolStr::from(c.to_string()))
        } else {
            None
        },
    })
}

/// Translate a mouse-move to a `BlitzInputEvent::PointerMove`.
pub(crate) fn neutral_move_to_blitz(x: i32, y: i32) -> BlitzInputEvent {
    BlitzInputEvent::PointerMove(make_pointer_event(
        x as f32,
        y as f32,
        MouseEventButton::Main,
        MouseEventButtons::None,
    ))
}

/// Translate a mouse click to a `BlitzInputEvent::PointerDown` or `PointerUp`.
pub(crate) fn neutral_click_to_blitz(
    x: i32,
    y: i32,
    button: MouseButton,
    mouse_up: bool,
) -> BlitzInputEvent {
    let (btn, btns) = match button {
        MouseButton::Left => (MouseEventButton::Main, MouseEventButtons::Primary),
        MouseButton::Middle => (MouseEventButton::Auxiliary, MouseEventButtons::Auxiliary),
        MouseButton::Right | MouseButton::Other(_) => {
            (MouseEventButton::Secondary, MouseEventButtons::Secondary)
        }
    };
    let ev = make_pointer_event_with_btn(x as f32, y as f32, btn, btns);
    if mouse_up {
        BlitzInputEvent::PointerUp(ev)
    } else {
        BlitzInputEvent::PointerDown(ev)
    }
}

/// Translate a mouse-leave to `BlitzInputEvent::PointerLeave`.
pub(crate) fn neutral_leave_to_blitz() -> BlitzInputEvent {
    BlitzInputEvent::PointerLeave
}

/// Translate a scroll wheel event to `BlitzInputEvent::Wheel`.
pub(crate) fn neutral_scroll_to_blitz(
    x: i32,
    y: i32,
    delta_x: i32,
    delta_y: i32,
) -> BlitzInputEvent {
    let coords = make_coords(x as f32, y as f32);
    BlitzInputEvent::Wheel(BlitzWheelEvent {
        delta: BlitzWheelDelta::Lines(delta_x as f64, delta_y as f64),
        coords,
        buttons: MouseEventButtons::None,
        mods: Modifiers::empty(),
    })
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn make_coords(x: f32, y: f32) -> PointerCoords {
    PointerCoords {
        page_x: x,
        page_y: y,
        screen_x: x,
        screen_y: y,
        client_x: x,
        client_y: y,
    }
}

fn make_pointer_event(
    x: f32,
    y: f32,
    button: MouseEventButton,
    buttons: MouseEventButtons,
) -> BlitzPointerEvent {
    make_pointer_event_with_btn(x, y, button, buttons)
}

fn make_pointer_event_with_btn(
    x: f32,
    y: f32,
    button: MouseEventButton,
    buttons: MouseEventButtons,
) -> BlitzPointerEvent {
    BlitzPointerEvent {
        id: BlitzPointerId::Mouse,
        is_primary: true,
        coords: make_coords(x, y),
        button,
        buttons,
        mods: Modifiers::empty(),
        details: PointerDetails::default(),
    }
}
