//! Conversion helpers: neutral input types → CEF input types.
//!
//! Private module. The public surface of `buffr-cef` only accepts
//! [`buffr_engine::NeutralKeyEvent`] and [`buffr_engine::MouseButton`];
//! this module performs the translation internally.

use buffr_engine::{KeyEventKind, MouseButton, NeutralKeyEvent};

/// Translate a [`NeutralKeyEvent`] into a [`cef::KeyEvent`].
pub(crate) fn neutral_to_cef_key(ev: NeutralKeyEvent) -> cef::KeyEvent {
    use cef::KeyEventType;
    let type_ = match ev.kind {
        KeyEventKind::RawDown => KeyEventType::RAWKEYDOWN,
        KeyEventKind::Char => KeyEventType::CHAR,
        KeyEventKind::Up => KeyEventType::KEYUP,
    };
    cef::KeyEvent {
        type_,
        modifiers: ev.modifiers,
        windows_key_code: ev.windows_key_code,
        native_key_code: ev.native_key_code,
        is_system_key: if ev.is_system_key { 1 } else { 0 },
        character: ev.character,
        unmodified_character: ev.unmodified_character,
        focus_on_editable_field: if ev.focus_on_editable_field { 1 } else { 0 },
        ..cef::KeyEvent::default()
    }
}

/// Translate a [`MouseButton`] into a [`cef::MouseButtonType`].
///
/// `MouseButton::Other(_)` maps to `LEFT` as a safe fallback (CEF does
/// not have an "Other" variant). A debug log is emitted on the fallback
/// path so the mapping gap is visible in traces.
pub(crate) fn neutral_to_cef_button(button: MouseButton) -> cef::MouseButtonType {
    match button {
        MouseButton::Left => cef::MouseButtonType::LEFT,
        MouseButton::Middle => cef::MouseButtonType::MIDDLE,
        MouseButton::Right => cef::MouseButtonType::RIGHT,
        MouseButton::Other(n) => {
            tracing::debug!(
                button = n,
                "neutral_to_cef_button: Other({n}) — no CEF equivalent, mapping to LEFT"
            );
            cef::MouseButtonType::LEFT
        }
    }
}
