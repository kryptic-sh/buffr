//! Bridge `KeyEvent` → modal [`KeyChord`] adapter.
//!
//! Gated behind the `bridge` Cargo feature. The bridge types defined
//! here match the public surface of [`wayr::KeyEvent`] exactly — same
//! fields, same semantics — but live in `buffr-modal` so consumers on
//! platforms where `wayr` can't compile (macOS / Windows, where
//! `wayland-client`'s pkg-config probe fails) can still translate key
//! events into the modal `KeyChord` representation.
//!
//! The mapping logic is identical in semantics to [`crate::wayr_adapter`].
//! Only the source struct is owned here.

use crate::key::{Key, KeyChord, Modifiers, NamedKey};

// ── Bridge types (mirror wayr::keyboard) ────────────────────────────────────

/// Physical scancode. Opaque outside the source platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScanCode(pub u32);

/// Logical key identification. Variants mirror wayr's shape: a
/// symbolic name (xkbcommon-style; e.g. `"Return"`, `"BackSpace"`,
/// `"F1"`) or a raw keysym number for keys outside the named set.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum KeyCode {
    Named(String),
    Sym(u32),
}

/// Modifier state. Caps / Num lock tracked separately from the chord
/// modifiers (they're toggles, not "held" modifiers in the vim sense).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct BridgeModifiers {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub logo: bool,
    pub caps_lock: bool,
    pub num_lock: bool,
}

/// Press / release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyState {
    Pressed,
    Released,
}

/// A single keyboard event. Source-shape parity with `wayr::KeyEvent`.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct KeyEvent {
    pub scancode: ScanCode,
    pub key_code: KeyCode,
    pub modifiers: BridgeModifiers,
    pub state: KeyState,
    pub text: Option<String>,
    pub repeat: bool,
}

impl KeyEvent {
    /// Construct a `KeyEvent` directly. Intended for unit tests and
    /// for the buffr-app windowing bridge that constructs these from
    /// winit events on non-Linux targets.
    pub fn new(
        scancode: ScanCode,
        key_code: KeyCode,
        modifiers: BridgeModifiers,
        state: KeyState,
        text: Option<String>,
        repeat: bool,
    ) -> Self {
        Self {
            scancode,
            key_code,
            modifiers,
            state,
            text,
            repeat,
        }
    }
}

// ── Chord translation (mirrors wayr_adapter::chord_from_event) ─────────────

/// Convert a bridge `KeyEvent` into a [`KeyChord`]. Returns `None` for
/// releases, repeats, and anything outside the chord set we route
/// through the trie (multi-codepoint composition, modifier-only
/// presses, unmapped keysyms).
pub fn key_event_to_chord(event: &KeyEvent) -> Option<KeyChord> {
    if event.state != KeyState::Pressed {
        return None;
    }
    if event.repeat {
        return None;
    }
    chord_from_event(event)
}

/// Like [`key_event_to_chord`] but accepts auto-repeat events. Used
/// by text-input surfaces (omnibar, command line) where holding
/// backspace or a character key fires continuously.
pub fn key_event_to_chord_with_repeat(event: &KeyEvent) -> Option<KeyChord> {
    if event.state != KeyState::Pressed {
        return None;
    }
    chord_from_event(event)
}

fn chord_from_event(event: &KeyEvent) -> Option<KeyChord> {
    let mods = modifiers_to_internal(event.modifiers);

    if let Some(text) = event.text.as_deref()
        && !text.is_empty()
    {
        let mut chars = text.chars();
        let first = chars.next()?;
        if chars.next().is_some() {
            return None;
        }
        let mut effective = mods;
        let mut ch = first;
        if effective.contains(Modifiers::SHIFT) && first.is_ascii() && !first.is_ascii_alphabetic()
        {
            effective.remove(Modifiers::SHIFT);
        }
        if effective.contains(Modifiers::CTRL) && ch.is_ascii_alphabetic() {
            ch = ch.to_ascii_lowercase();
        }
        return Some(KeyChord {
            modifiers: effective,
            key: Key::Char(ch),
        });
    }

    if let KeyCode::Named(name) = &event.key_code {
        if name == "space" || name == " " {
            return Some(KeyChord {
                modifiers: mods,
                key: Key::Char(' '),
            });
        }
        if let Some(mapped) = map_named(name) {
            return Some(KeyChord {
                modifiers: mods,
                key: Key::Named(mapped),
            });
        }
        // Defense-in-depth: winit stuffs `logical_key.Character("0")`
        // into `KeyCode::Named("0")` (the bridge KeyCode enum has no
        // Char variant). If `text` was empty and the named-key table
        // doesn't recognise the name, fall back to treating a single
        // printable codepoint as a Char chord. Matches the vim
        // bindings on the digit / punctuation rows (`0`, `-`, `=`).
        let mut chars = name.chars();
        if let (Some(first), None) = (chars.next(), chars.next())
            && !first.is_ascii_control()
        {
            let mut effective = mods;
            let mut ch = first;
            if effective.contains(Modifiers::SHIFT)
                && first.is_ascii()
                && !first.is_ascii_alphabetic()
            {
                effective.remove(Modifiers::SHIFT);
            }
            if effective.contains(Modifiers::CTRL) && ch.is_ascii_alphabetic() {
                ch = ch.to_ascii_lowercase();
            }
            return Some(KeyChord {
                modifiers: effective,
                key: Key::Char(ch),
            });
        }
    }
    None
}

fn modifiers_to_internal(m: BridgeModifiers) -> Modifiers {
    let mut out = Modifiers::empty();
    if m.shift {
        out |= Modifiers::SHIFT;
    }
    if m.ctrl {
        out |= Modifiers::CTRL;
    }
    if m.alt {
        out |= Modifiers::ALT;
    }
    if m.logo {
        out |= Modifiers::SUPER;
    }
    out
}

fn map_named(name: &str) -> Option<NamedKey> {
    Some(match name {
        "Escape" => NamedKey::Esc,
        "Return" | "KP_Enter" => NamedKey::CR,
        "Tab" | "ISO_Left_Tab" => NamedKey::Tab,
        "BackSpace" => NamedKey::BS,
        "space" => NamedKey::Space,
        "Up" => NamedKey::Up,
        "Down" => NamedKey::Down,
        "Left" => NamedKey::Left,
        "Right" => NamedKey::Right,
        "Home" => NamedKey::Home,
        "End" => NamedKey::End,
        "Prior" | "Page_Up" => NamedKey::PageUp,
        "Next" | "Page_Down" => NamedKey::PageDown,
        "Insert" => NamedKey::Insert,
        "Delete" => NamedKey::Delete,
        "F1" => NamedKey::F(1),
        "F2" => NamedKey::F(2),
        "F3" => NamedKey::F(3),
        "F4" => NamedKey::F(4),
        "F5" => NamedKey::F(5),
        "F6" => NamedKey::F(6),
        "F7" => NamedKey::F(7),
        "F8" => NamedKey::F(8),
        "F9" => NamedKey::F(9),
        "F10" => NamedKey::F(10),
        "F11" => NamedKey::F(11),
        "F12" => NamedKey::F(12),
        _ => return None,
    })
}
