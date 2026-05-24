//! Keyboard input types + winit → bridge mapping helpers.
//!
//! The bridge `KeyEvent` / `KeyCode` / `Modifiers` / `KeyState` /
//! `ScanCode` types live in `buffr-modal::bridge_adapter` so that
//! `buffr-modal::bridge_key_event_to_chord` can take them by
//! reference. Re-exported here so `crate::windowing::*` reads the
//! same on every platform.
//!
//! `ScanCode` is the raw Linux evdev scancode on wayr; on winit we
//! map from [`winit::keyboard::KeyCode`] to the equivalent evdev
//! value so the `scan_code_to_vk` table in main.rs keeps working
//! unchanged.

pub use buffr_modal::{
    BridgeKeyCode as KeyCode, BridgeKeyState as KeyState, BridgeModifiers as Modifiers,
    BridgeScanCode as ScanCode,
};
// `BridgeKeyEvent` keeps its `new(...)` constructor; we don't need a
// separate `new_for_test` shim since main.rs uses the constructor
// directly.
pub use buffr_modal::BridgeKeyEvent as KeyEvent;

// ── winit → bridge mappings ──────────────────────────────────────────────

/// Map a winit `ModifiersState` snapshot into the bridge `Modifiers`.
/// Caps-lock / num-lock are not tracked separately by winit 0.30 — those
/// stay `false` (the consumer's modifier-aware paths only read
/// `shift`/`ctrl`/`alt`/`logo`).
pub(super) fn modifiers_from_winit(state: winit::keyboard::ModifiersState) -> Modifiers {
    Modifiers {
        shift: state.shift_key(),
        ctrl: state.control_key(),
        alt: state.alt_key(),
        logo: state.super_key(),
        caps_lock: false,
        num_lock: false,
    }
}

/// Map a winit `PhysicalKey` to an evdev scancode. Returns `0` for
/// unrecognised keys (matches wayr's behaviour for unknown keysyms).
pub(super) fn scancode_from_winit_physical(physical: winit::keyboard::PhysicalKey) -> ScanCode {
    use winit::keyboard::KeyCode as W;
    let raw = match physical {
        winit::keyboard::PhysicalKey::Code(code) => match code {
            // Function row.
            W::Escape => 1,
            // Number row.
            W::Digit1 => 2,
            W::Digit2 => 3,
            W::Digit3 => 4,
            W::Digit4 => 5,
            W::Digit5 => 6,
            W::Digit6 => 7,
            W::Digit7 => 8,
            W::Digit8 => 9,
            W::Digit9 => 10,
            W::Digit0 => 11,
            W::Minus => 12,
            W::Equal => 13,
            W::Backspace => 14,
            W::Tab => 15,
            // QWERTY row.
            W::KeyQ => 16,
            W::KeyW => 17,
            W::KeyE => 18,
            W::KeyR => 19,
            W::KeyT => 20,
            W::KeyY => 21,
            W::KeyU => 22,
            W::KeyI => 23,
            W::KeyO => 24,
            W::KeyP => 25,
            W::BracketLeft => 26,
            W::BracketRight => 27,
            W::Enter => 28,
            W::ControlLeft => 29,
            // ASDF row.
            W::KeyA => 30,
            W::KeyS => 31,
            W::KeyD => 32,
            W::KeyF => 33,
            W::KeyG => 34,
            W::KeyH => 35,
            W::KeyJ => 36,
            W::KeyK => 37,
            W::KeyL => 38,
            W::Semicolon => 39,
            W::Quote => 40,
            W::Backquote => 41,
            W::ShiftLeft => 42,
            W::Backslash => 43,
            // ZXCV row.
            W::KeyZ => 44,
            W::KeyX => 45,
            W::KeyC => 46,
            W::KeyV => 47,
            W::KeyB => 48,
            W::KeyN => 49,
            W::KeyM => 50,
            W::Comma => 51,
            W::Period => 52,
            W::Slash => 53,
            W::ShiftRight => 54,
            W::AltLeft => 56,
            W::Space => 57,
            W::CapsLock => 58,
            // F-keys.
            W::F1 => 59,
            W::F2 => 60,
            W::F3 => 61,
            W::F4 => 62,
            W::F5 => 63,
            W::F6 => 64,
            W::F7 => 65,
            W::F8 => 66,
            W::F9 => 67,
            W::F10 => 68,
            W::F11 => 87,
            W::F12 => 88,
            // Navigation cluster.
            W::Home => 102,
            W::ArrowUp => 103,
            W::PageUp => 104,
            W::ArrowLeft => 105,
            W::ArrowRight => 106,
            W::End => 107,
            W::ArrowDown => 108,
            W::PageDown => 109,
            W::Insert => 110,
            W::Delete => 111,
            W::ControlRight => 97,
            W::AltRight => 100,
            W::SuperLeft => 125,
            W::SuperRight => 126,
            _ => 0,
        },
        winit::keyboard::PhysicalKey::Unidentified(_) => 0,
    };
    ScanCode(raw)
}

/// Map a winit `Key` (logical) into the bridge `KeyCode`. Mirrors the
/// xkb-style symbolic names wayr emits — the existing match arms in
/// main.rs (`"Escape"`, `"Return"`, `"BackSpace"`, …) keep working.
pub(super) fn key_code_from_winit_logical(logical: &winit::keyboard::Key) -> KeyCode {
    use winit::keyboard::{Key, NamedKey};
    match logical {
        Key::Named(named) => {
            let name = match named {
                NamedKey::Escape => "Escape",
                NamedKey::Enter => "Return",
                NamedKey::Tab => "Tab",
                NamedKey::Backspace => "BackSpace",
                NamedKey::Delete => "Delete",
                NamedKey::ArrowUp => "Up",
                NamedKey::ArrowDown => "Down",
                NamedKey::ArrowLeft => "Left",
                NamedKey::ArrowRight => "Right",
                NamedKey::Home => "Home",
                NamedKey::End => "End",
                NamedKey::PageUp => "Prior",
                NamedKey::PageDown => "Next",
                NamedKey::Insert => "Insert",
                NamedKey::F1 => "F1",
                NamedKey::F2 => "F2",
                NamedKey::F3 => "F3",
                NamedKey::F4 => "F4",
                NamedKey::F5 => "F5",
                NamedKey::F6 => "F6",
                NamedKey::F7 => "F7",
                NamedKey::F8 => "F8",
                NamedKey::F9 => "F9",
                NamedKey::F10 => "F10",
                NamedKey::F11 => "F11",
                NamedKey::F12 => "F12",
                NamedKey::Space => " ",
                NamedKey::Shift => "Shift_L",
                NamedKey::Control => "Control_L",
                NamedKey::Alt => "Alt_L",
                NamedKey::Super => "Super_L",
                NamedKey::CapsLock => "Caps_Lock",
                NamedKey::NumLock => "Num_Lock",
                _ => return KeyCode::Named(format!("{named:?}")),
            };
            KeyCode::Named(name.to_string())
        }
        Key::Character(s) => KeyCode::Named(s.as_str().to_string()),
        Key::Unidentified(_) => KeyCode::Sym(0),
        Key::Dead(_) => KeyCode::Sym(0),
    }
}
