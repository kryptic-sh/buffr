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
