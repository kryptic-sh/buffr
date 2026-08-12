//! Translation between the `windowing` (wayr / winit) input types and
//! the CEF and `buffr_engine` types they feed.
//!
//! These are all pure mapping functions with no access to `AppState`;
//! they exist as a unit because the OSR input path has to convert
//! every event twice — once out of the platform's vocabulary and once
//! into CEF's — and the two halves are easier to keep consistent when
//! they sit together.

use crate::windowing::{CursorIcon, Modifiers};

// ---- OSR input helpers ---------------------------------------------------

/// Convert a wayr `ScrollEvent` to a CEF wheel delta (dx, dy, is_pixel).
///
/// CEF's `send_mouse_wheel_event` takes integer deltas in wheel-tick units
/// (~120 = 1 line). Touchpad / high-res sources produce sub-pixel deltas;
/// we scale those by `PIXEL_DELTA_SCALE` (10× — empirical sweet spot for
/// touchpad feel after testing). Discrete wheel ticks use the `discrete_steps`
/// field (×120 per step).
///
/// wayr `ScrollEvent` carries a single axis per event; the orthogonal axis
/// delta is always 0.
///
/// Sign convention: wayr matches winit — positive vertical = scroll up,
/// positive horizontal = scroll right. CEF `send_mouse_wheel_event` uses
/// the same convention, so no sign flip is needed here.
pub(crate) fn scroll_to_cef_delta(ev: &crate::windowing::ScrollEvent) -> (i32, i32, bool) {
    use crate::windowing::{AxisDirection, AxisSource};
    const PIXEL_DELTA_SCALE: f32 = 10.0;
    let is_pixel = matches!(ev.source, AxisSource::Finger | AxisSource::Continuous);
    let scaled = if is_pixel {
        (ev.delta as f32 * PIXEL_DELTA_SCALE) as i32
    } else {
        // Discrete: prefer discrete_steps if available, else use delta.
        if ev.discrete_steps != 0 {
            ev.discrete_steps * 120
        } else {
            (ev.delta as f32 * 120.0) as i32
        }
    };
    match ev.axis {
        AxisDirection::Horizontal => (scaled, 0, is_pixel),
        AxisDirection::Vertical => (0, scaled, is_pixel),
    }
}

/// Map a CEF `CursorType` raw discriminant to a winit [`CursorIcon`].
///
/// CEF emits the type via `DisplayHandler::on_cursor_change` whenever the
/// page wants the system cursor to update (link hover → hand, input hover
/// → ibeam, resize edge → corresponding resize arrow, …). The raw value is
/// `CursorType::get_raw()` — kept opaque here so buffr-core stays free of
/// winit deps.
///
/// Constants come from `cef_dll_sys::cef_cursor_type_t` (stable across
/// CEF versions; no import needed here — we compare raw integers).
///
/// Unknown / unimplemented variants fall back to [`CursorIcon::Default`].
pub(crate) fn cef_cursor_to_icon(raw: u32) -> CursorIcon {
    // Raw values from cef_dll_sys::cef_cursor_type_t (CEF 147, stable).
    const CT_POINTER: u32 = 0;
    const CT_CROSS: u32 = 1;
    const CT_HAND: u32 = 2;
    const CT_IBEAM: u32 = 3;
    const CT_WAIT: u32 = 4;
    const CT_HELP: u32 = 5;
    const CT_EASTRESIZE: u32 = 6;
    const CT_NORTHRESIZE: u32 = 7;
    const CT_NORTHEASTRESIZE: u32 = 8;
    const CT_NORTHWESTRESIZE: u32 = 9;
    const CT_SOUTHRESIZE: u32 = 10;
    const CT_SOUTHEASTRESIZE: u32 = 11;
    const CT_SOUTHWESTRESIZE: u32 = 12;
    const CT_WESTRESIZE: u32 = 13;
    const CT_NORTHSOUTHRESIZE: u32 = 14;
    const CT_EASTWESTRESIZE: u32 = 15;
    const CT_NORTHEASTSOUTHWESTRESIZE: u32 = 16;
    const CT_NORTHWESTSOUTHEASTRESIZE: u32 = 17;
    const CT_COLUMNRESIZE: u32 = 18;
    const CT_ROWRESIZE: u32 = 19;
    const CT_MOVE: u32 = 20;
    const CT_VERTICALTEXT: u32 = 21;
    const CT_CELL: u32 = 22;
    const CT_CONTEXTMENU: u32 = 23;
    const CT_ALIAS: u32 = 24;
    const CT_PROGRESS: u32 = 25;
    const CT_NODROP: u32 = 26;
    const CT_COPY: u32 = 27;
    const CT_NONE: u32 = 28;
    const CT_NOTALLOWED: u32 = 29;
    const CT_ZOOMIN: u32 = 30;
    const CT_ZOOMOUT: u32 = 31;
    const CT_GRAB: u32 = 32;
    const CT_GRABBING: u32 = 33;
    const CT_DND_NONE: u32 = 34;
    const CT_DND_MOVE: u32 = 35;
    const CT_DND_COPY: u32 = 36;
    const CT_DND_LINK: u32 = 37;

    match raw {
        CT_POINTER => CursorIcon::Default,
        CT_CROSS => CursorIcon::Crosshair,
        CT_HAND => CursorIcon::Pointer,
        CT_IBEAM => CursorIcon::Text,
        CT_WAIT => CursorIcon::Wait,
        CT_HELP => CursorIcon::Help,
        CT_EASTRESIZE => CursorIcon::EResize,
        CT_NORTHRESIZE => CursorIcon::NResize,
        CT_NORTHEASTRESIZE => CursorIcon::NeResize,
        CT_NORTHWESTRESIZE => CursorIcon::NwResize,
        CT_SOUTHRESIZE => CursorIcon::SResize,
        CT_SOUTHEASTRESIZE => CursorIcon::SeResize,
        CT_SOUTHWESTRESIZE => CursorIcon::SwResize,
        CT_WESTRESIZE => CursorIcon::WResize,
        CT_NORTHSOUTHRESIZE => CursorIcon::NsResize,
        CT_EASTWESTRESIZE => CursorIcon::EwResize,
        CT_NORTHEASTSOUTHWESTRESIZE => CursorIcon::NeswResize,
        CT_NORTHWESTSOUTHEASTRESIZE => CursorIcon::NwseResize,
        CT_COLUMNRESIZE => CursorIcon::ColResize,
        CT_ROWRESIZE => CursorIcon::RowResize,
        CT_MOVE => CursorIcon::Move,
        CT_VERTICALTEXT => CursorIcon::VerticalText,
        CT_CELL => CursorIcon::Cell,
        CT_CONTEXTMENU => CursorIcon::ContextMenu,
        CT_ALIAS => CursorIcon::Alias,
        CT_PROGRESS => CursorIcon::Progress,
        CT_NODROP | CT_NOTALLOWED => CursorIcon::NotAllowed,
        CT_COPY | CT_DND_COPY => CursorIcon::Copy,
        CT_NONE => {
            // winit has no "hide cursor" CursorIcon variant; closest match.
            CursorIcon::Default
        }
        CT_ZOOMIN => CursorIcon::ZoomIn,
        CT_ZOOMOUT => CursorIcon::ZoomOut,
        CT_GRAB => CursorIcon::Grab,
        CT_GRABBING => CursorIcon::Grabbing,
        CT_DND_NONE => CursorIcon::NotAllowed,
        CT_DND_MOVE => CursorIcon::Move,
        CT_DND_LINK => CursorIcon::Alias,
        _ => CursorIcon::Default,
    }
}

/// Convert wayr `Modifiers` to CEF event-flag bits.
///
/// CEF bit values (from cef_dll_sys `cef_event_flags_t`):
///   SHIFT   = 2
///   CONTROL = 4
///   ALT     = 8
///   COMMAND = 128
pub(crate) fn mods_to_cef(m: &Modifiers) -> u32 {
    let mut flags: u32 = 0;
    if m.shift {
        flags |= 2;
    }
    if m.ctrl {
        flags |= 4;
    }
    if m.alt {
        flags |= 8;
    }
    if m.logo {
        flags |= 128;
    }
    flags
}

/// Map a wayr `ScanCode` to a Windows virtual-key code for CEF.
///
/// wayr `ScanCode` carries the raw Linux evdev scancode (matches
/// `linux/input-event-codes.h` — `KEY_ESC = 1`, `KEY_BACKSPACE = 14`,
/// `KEY_TAB = 15`, `KEY_ENTER = 28`, …); no offset adjustment needed.
/// Coverage: A-Z, 0-9, F1-F12, common navigation and editing keys,
/// plus OEM punctuation so virtual-keyboard tools (wtype, xdotool etc.)
/// that route through `zwp_virtual_keyboard_v1` still deliver VK codes.
/// Unknowns map to 0 (CEF ignores `windows_key_code == 0` for non-printable
/// keys; printable keys use `character` instead).
pub(crate) fn scan_code_to_vk(sc: crate::windowing::ScanCode) -> i32 {
    // wayr surfaces the raw evdev scancode directly — no offset.
    match sc.0 {
        // Row 1 — number row (evdev 2-13).
        2 => 0x31,  // 1
        3 => 0x32,  // 2
        4 => 0x33,  // 3
        5 => 0x34,  // 4
        6 => 0x35,  // 5
        7 => 0x36,  // 6
        8 => 0x37,  // 7
        9 => 0x38,  // 8
        10 => 0x39, // 9
        11 => 0x30, // 0
        12 => 0xBD, // minus (-) VK_OEM_MINUS
        13 => 0xBB, // equal (=) VK_OEM_PLUS
        // Editing (evdev 14, 15).
        14 => 0x08, // Backspace VK_BACK
        15 => 0x09, // Tab VK_TAB
        // QWERTY row (evdev 16-27).
        16 => 0x51, // q
        17 => 0x57, // w
        18 => 0x45, // e
        19 => 0x52, // r
        20 => 0x54, // t
        21 => 0x59, // y
        22 => 0x55, // u
        23 => 0x49, // i
        24 => 0x4F, // o
        25 => 0x50, // p
        26 => 0xDB, // [ VK_OEM_4
        27 => 0xDD, // ] VK_OEM_6
        28 => 0x0D, // Enter VK_RETURN
        // ASDF row (evdev 30-41).
        30 => 0x41, // a
        31 => 0x53, // s
        32 => 0x44, // d
        33 => 0x46, // f
        34 => 0x47, // g
        35 => 0x48, // h
        36 => 0x4A, // j
        37 => 0x4B, // k
        38 => 0x4C, // l
        39 => 0xBA, // ; VK_OEM_1
        40 => 0xDE, // ' VK_OEM_7
        41 => 0xC0, // ` VK_OEM_3
        43 => 0xDC, // \ VK_OEM_5
        // ZXCV row (evdev 44-53).
        44 => 0x5A, // z
        45 => 0x58, // x
        46 => 0x43, // c
        47 => 0x56, // v
        48 => 0x42, // b
        49 => 0x4E, // n
        50 => 0x4D, // m
        51 => 0xBC, // , VK_OEM_COMMA
        52 => 0xBE, // . VK_OEM_PERIOD
        53 => 0xBF, // / VK_OEM_2
        57 => 0x20, // Space VK_SPACE
        // F-keys (evdev 59-68 = F1-F10, 87-88 = F11-F12).
        59 => 0x70, // F1
        60 => 0x71, // F2
        61 => 0x72, // F3
        62 => 0x73, // F4
        63 => 0x74, // F5
        64 => 0x75, // F6
        65 => 0x76, // F7
        66 => 0x77, // F8
        67 => 0x78, // F9
        68 => 0x79, // F10
        87 => 0x7A, // F11
        88 => 0x7B, // F12
        // Navigation cluster.
        102 => 0x24, // Home
        103 => 0x26, // ArrowUp
        104 => 0x21, // PageUp
        105 => 0x25, // ArrowLeft
        106 => 0x27, // ArrowRight
        107 => 0x23, // End
        108 => 0x28, // ArrowDown
        109 => 0x22, // PageDown
        110 => 0x2D, // Insert
        111 => 0x2E, // Delete
        // Escape (evdev 1).
        1 => 0x1B, // Escape VK_ESCAPE
        _ => 0,
    }
}

/// Resolve the CHAR-event code unit for a wayr key event.
///
/// Uses `event.text` (the IME-translated / xkb-composed string).
/// For keys without a printable character (arrows, F-keys, modifiers),
/// wayr sets `text` to `None` and we return 0.
///
/// Returns 0 when no single-UTF-16-unit character is available (multi-unit
/// chars, named keys, modifier-only events).
pub(crate) fn resolve_char_unit(text: Option<&str>) -> u16 {
    text.and_then(|t| t.chars().next())
        .map(|c| {
            let mut buf = [0u16; 2];
            let encoded = c.encode_utf16(&mut buf);
            if encoded.len() == 1 { encoded[0] } else { 0 }
        })
        .unwrap_or(0)
}

/// The text of a key event that a CHAR event cannot carry: exactly one
/// character needing 2 UTF-16 units (a surrogate pair — emoji, rare CJK).
///
/// Sending such a character as key events drops it: `resolve_char_unit`
/// returns 0 for it, and a lone surrogate unit renders as U+FFFD. Callers
/// must insert the text via `execCommand('insertText')` instead (the same
/// delivery Ctrl+V uses). Returns `None` for everything else — ASCII,
/// BMP chars, named keys, multi-char strings.
pub(crate) fn multi_unit_char_text(text: Option<&str>) -> Option<&str> {
    text.and_then(|t| {
        let mut chars = t.chars();
        let c = chars.next()?;
        if chars.next().is_none() && c.len_utf16() == 2 {
            Some(t)
        } else {
            None
        }
    })
}

/// Map a printable ASCII character to its Windows VK code.
///
/// Used when the typed character disagrees with the physical scancode —
/// the wtype / xdotool / accessibility-tool case. Those tools build a
/// synthetic xkb keymap that assigns each character to whichever scancode
/// is convenient (`Escape`, `Digit1`, `Tab`, …); blindly translating the
/// scancode would make Chromium fire keydown with `code=Escape` /
/// `code=Tab` / `code=Backspace` while the character is actually `s` /
/// `o` / `c`. Effects: Escape blurs the input, Tab jumps focus, Backspace
/// deletes the previous character. Match by character instead so virtual
/// keyboards send the VK that lines up with the text.
///
/// Returns `None` for characters with no direct VK (shifted punctuation
/// like `@` `#` `$`, non-ASCII), letting the caller keep the
/// scancode-derived VK as a fallback.
pub(crate) fn char_to_vk(ch: u16) -> Option<i32> {
    let c = char::from_u32(ch as u32)?;
    Some(match c {
        'a'..='z' => (c as u32 - 'a' as u32 + 0x41) as i32, // VK_A..VK_Z
        'A'..='Z' => c as i32,                              // VK_A..VK_Z
        '0'..='9' => c as i32,                              // VK_0..VK_9
        ' ' => 0x20,                                        // VK_SPACE
        '\r' => 0x0D,                                       // VK_RETURN
        '\n' => 0x0D,
        '\t' => 0x09,     // VK_TAB
        '\x08' => 0x08,   // VK_BACK
        '\x1b' => 0x1B,   // VK_ESCAPE
        '.' => 0xBE,      // VK_OEM_PERIOD
        ',' => 0xBC,      // VK_OEM_COMMA
        '-' => 0xBD,      // VK_OEM_MINUS
        '=' => 0xBB,      // VK_OEM_PLUS
        ';' => 0xBA,      // VK_OEM_1
        '/' => 0xBF,      // VK_OEM_2
        '`' => 0xC0,      // VK_OEM_3
        '[' => 0xDB,      // VK_OEM_4
        '\\' => 0xDC,     // VK_OEM_5
        ']' => 0xDD,      // VK_OEM_6
        '\'' => 0xDE,     // VK_OEM_7
        _ => return None, // shifted symbols (@ # $ % …), non-ASCII
    })
}

/// Build neutral [`NeutralKeyEvent`]s from a wayr key event.
///
/// `focus_on_editable_field` reports whether a text input is currently
/// focused — Chromium routes editable-field shortcuts and composition
/// state differently when this flag is set, and some virtual-keyboard
/// pathways need it to dispatch keystrokes through the same DOM event
/// flow as real-keyboard typing.
///
/// Returns an empty vec for modifier-only presses (no VK code, no character).
pub(crate) fn key_to_neutral_events(
    event: &crate::windowing::KeyEvent,
    modifiers: u32,
    focus_on_editable_field: bool,
) -> Vec<buffr_engine::NeutralKeyEvent> {
    use buffr_engine::{KeyEventKind, NeutralKeyEvent};

    let vk_from_sc = scan_code_to_vk(event.scancode);
    let ch = resolve_char_unit(event.text.as_deref());
    // Prefer a VK derived from the resolved character when one is
    // available — virtual_keyboard sources (wtype etc.) put characters
    // on arbitrary scancodes, so the physical mapping would otherwise
    // deliver e.g. `VK_BACK` with character `'c'`. Fall through to the
    // scancode-derived VK for shifted symbols / non-ASCII / no-text
    // events (real-keyboard typing matches both, so this is a no-op).
    let vk = char_to_vk(ch).unwrap_or(vk_from_sc);

    // Skip pure modifier keys (no VK, no character text).
    if vk == 0 && ch == 0 {
        return Vec::new();
    }

    // Shared field values for every event we emit; each arm only
    // overrides `kind` (and `windows_key_code` for the Char event).
    let base = NeutralKeyEvent {
        kind: KeyEventKind::RawDown,
        windows_key_code: vk,
        native_key_code: 0,
        character: ch,
        unmodified_character: ch,
        modifiers,
        is_system_key: false,
        focus_on_editable_field,
    };
    match event.state {
        crate::windowing::KeyState::Pressed => {
            let raw = NeutralKeyEvent { ..base };
            if ch != 0 {
                let char_ev = NeutralKeyEvent {
                    kind: KeyEventKind::Char,
                    windows_key_code: ch as i32,
                    ..base
                };
                vec![raw, char_ev]
            } else {
                vec![raw]
            }
        }
        crate::windowing::KeyState::Released => {
            vec![NeutralKeyEvent {
                kind: KeyEventKind::Up,
                ..base
            }]
        }
    }
}

/// Map a wayr `PointerButton` to a neutral [`buffr_engine::MouseButton`].
/// Returns `None` for `Other(_)` buttons — callers that need a fallback
/// use [`buffr_engine::MouseButton::Other`] directly.
pub(crate) fn button_to_neutral(
    button: &crate::windowing::PointerButton,
) -> Option<buffr_engine::MouseButton> {
    use crate::windowing::PointerButton;
    match button {
        PointerButton::Left => Some(buffr_engine::MouseButton::Left),
        PointerButton::Right => Some(buffr_engine::MouseButton::Right),
        PointerButton::Middle => Some(buffr_engine::MouseButton::Middle),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multi_unit_char_text_detects_surrogate_pairs() {
        // §11-16: an emoji is one char needing 2 UTF-16 units — the key
        // event channel cannot carry it and must fall back to text insert.
        assert_eq!(multi_unit_char_text(Some("😀")), Some("😀"));
        // Rare CJK outside the BMP.
        assert_eq!(multi_unit_char_text(Some("𠜎")), Some("𠜎"));
    }

    #[test]
    fn multi_unit_char_text_rejects_single_unit_and_others() {
        assert_eq!(multi_unit_char_text(Some("a")), None);
        assert_eq!(multi_unit_char_text(Some("é")), None); // BMP, 1 unit
        assert_eq!(multi_unit_char_text(Some("ab")), None); // multi-char
        assert_eq!(multi_unit_char_text(Some("😀x")), None); // pair + extra
        assert_eq!(multi_unit_char_text(None), None);
    }
}
