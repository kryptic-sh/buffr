//! Neutral input event types — no CEF types exposed.
//!
//! `buffr-app` builds these from winit events and passes them to the
//! engine. `buffr-cef` translates them to `cef::KeyEvent` /
//! `cef::MouseButtonType` internally.

/// Which phase of a keyboard event this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeyEventKind {
    /// Key pressed (no character): `RAWKEYDOWN` in CEF terms.
    #[default]
    RawDown,
    /// Character input: `CHAR` in CEF terms.
    Char,
    /// Key released: `KEYUP` in CEF terms.
    Up,
}

/// Engine-agnostic keyboard event.
///
/// Field names and semantics match CEF's `cef_key_event_t` but carry no
/// CEF types. `buffr-cef` converts to `cef::KeyEvent` on the way in.
///
/// `windows_key_code` and `character` are Windows virtual-key codes /
/// UTF-16 code units. On non-Windows platforms `buffr-app` maps winit
/// scancodes / logical keys to the same VK table, matching what CEF
/// expects on Linux and macOS.
#[derive(Debug, Clone, Copy, Default)]
pub struct NeutralKeyEvent {
    pub kind: KeyEventKind,
    /// Windows VK_* code. 0 for pure modifier presses.
    pub windows_key_code: i32,
    /// Platform native key code. 0 when unused.
    pub native_key_code: i32,
    /// UTF-16 character (for `Char` events). 0 when none.
    pub character: u16,
    /// Character ignoring modifiers. 0 when none.
    pub unmodified_character: u16,
    /// CEF EVENTFLAG_* bitmask (already a `u32` in CEF; kept as-is).
    pub modifiers: u32,
    /// Whether this is an Alt+key system shortcut.
    pub is_system_key: bool,
    /// Whether a text input is focused (affects Chromium's key routing).
    pub focus_on_editable_field: bool,
}

/// Neutral mouse-button identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    /// Catch-all for extended buttons. The inner value is an arbitrary
    /// platform integer; CEF backends map it to `LEFT` as a fallback.
    Other(u8),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neutral_key_event_default_zero_fields() {
        let ev = NeutralKeyEvent::default();
        assert_eq!(ev.windows_key_code, 0);
        assert_eq!(ev.native_key_code, 0);
        assert_eq!(ev.character, 0);
        assert_eq!(ev.unmodified_character, 0);
        assert_eq!(ev.modifiers, 0);
        assert!(!ev.is_system_key);
        assert!(!ev.focus_on_editable_field);
    }

    #[test]
    fn key_event_kind_default_is_rawdown() {
        assert_eq!(KeyEventKind::default(), KeyEventKind::RawDown);
    }

    #[test]
    fn mouse_button_left_right_middle_distinct() {
        assert_ne!(MouseButton::Left, MouseButton::Right);
        assert_ne!(MouseButton::Left, MouseButton::Middle);
        assert_ne!(MouseButton::Right, MouseButton::Middle);
    }

    #[test]
    fn mouse_button_other_variant_carries_value() {
        let btn = MouseButton::Other(7);
        assert_eq!(btn, MouseButton::Other(7));
        assert_ne!(btn, MouseButton::Other(8));
        assert_ne!(btn, MouseButton::Left);
    }

    #[test]
    fn key_event_kind_all_variants_match() {
        let variants = [KeyEventKind::RawDown, KeyEventKind::Char, KeyEventKind::Up];
        // Every variant is distinct.
        assert_eq!(variants[0], KeyEventKind::RawDown);
        assert_eq!(variants[1], KeyEventKind::Char);
        assert_eq!(variants[2], KeyEventKind::Up);
        assert_ne!(variants[0], variants[1]);
    }
}
