//! Bridge `KeyEvent` → modal [`KeyChord`] adapter.
//!
//! Gated behind the `bridge` Cargo feature. The bridge types defined
//! here match the public surface of [`wayr::KeyEvent`] exactly — same
//! fields, same semantics — but live in `buffr-modal` so consumers on
//! platforms where `wayr` can't compile (macOS / Windows, where
//! `wayland-client`'s pkg-config probe fails) can still translate key
//! events into the modal `KeyChord` representation.
//!
//! The mapping logic is literally the same code as
//! [`crate::wayr_adapter`]'s — both peel their source event down to
//! `(text, named, modifiers)` and hand it to the shared translator in
//! `crate::adapter`. Only the source struct is owned here.

use crate::adapter::chord_from_parts;
use crate::key::{KeyChord, Modifiers};

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

// ── Chord translation (shared with wayr_adapter) ───────────────────────────

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
    let named = match &event.key_code {
        KeyCode::Named(name) => Some(name.as_str()),
        KeyCode::Sym(_) => None,
    };
    chord_from_parts(
        event.text.as_deref(),
        named,
        modifiers_to_internal(event.modifiers),
    )
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key::{Key, NamedKey};

    fn ev(text: Option<&str>, named: &str, mods: BridgeModifiers) -> KeyEvent {
        KeyEvent::new(
            ScanCode(0),
            KeyCode::Named(named.to_string()),
            mods,
            KeyState::Pressed,
            text.map(str::to_string),
            false,
        )
    }

    #[test]
    fn plain_j_via_text() {
        let chord = key_event_to_chord(&ev(Some("j"), "j", BridgeModifiers::default())).unwrap();
        assert_eq!(chord.key, Key::Char('j'));
        assert!(chord.modifiers.is_empty());
    }

    #[test]
    fn shift_j_carries_uppercase_and_shift_flag() {
        let mods = BridgeModifiers {
            shift: true,
            ..Default::default()
        };
        let chord = key_event_to_chord(&ev(Some("J"), "J", mods)).unwrap();
        assert_eq!(chord.key, Key::Char('J'));
        assert!(chord.modifiers.contains(Modifiers::SHIFT));
    }

    #[test]
    fn shift_plus_drops_shift_modifier() {
        // `+` is the shifted form of `=` on US — the parser writes `+`
        // directly without `<S->`, so the adapter must shed SHIFT.
        let mods = BridgeModifiers {
            shift: true,
            ..Default::default()
        };
        let chord = key_event_to_chord(&ev(Some("+"), "plus", mods)).unwrap();
        assert_eq!(chord.key, Key::Char('+'));
        assert!(!chord.modifiers.contains(Modifiers::SHIFT));
    }

    #[test]
    fn ctrl_shift_h_normalizes_to_lowercase() {
        let mods = BridgeModifiers {
            ctrl: true,
            shift: true,
            ..Default::default()
        };
        let chord = key_event_to_chord(&ev(Some("H"), "H", mods)).unwrap();
        assert_eq!(chord.key, Key::Char('h'));
        assert!(chord.modifiers.contains(Modifiers::CTRL));
        assert!(chord.modifiers.contains(Modifiers::SHIFT));
    }

    #[test]
    fn named_keys_map_without_text() {
        for (name, expect) in [
            ("Escape", NamedKey::Esc),
            ("Return", NamedKey::CR),
            ("BackSpace", NamedKey::BS),
            ("Tab", NamedKey::Tab),
            ("Delete", NamedKey::Delete),
            ("Prior", NamedKey::PageUp),
            ("F7", NamedKey::F(7)),
        ] {
            let chord = key_event_to_chord(&ev(None, name, BridgeModifiers::default())).unwrap();
            assert_eq!(chord.key, Key::Named(expect), "{name}");
        }
    }

    #[test]
    fn space_alias_lands_on_char_space() {
        for name in ["space", " "] {
            let chord = key_event_to_chord(&ev(None, name, BridgeModifiers::default())).unwrap();
            assert_eq!(chord.key, Key::Char(' '), "{name:?}");
        }
    }

    #[test]
    fn single_printable_named_falls_back_to_char() {
        // winit stuffs `logical_key.Character("0")` into
        // `KeyCode::Named("0")`; the digit / punctuation rows must
        // still reach the trie.
        for ch in ['0', '-', '='] {
            let chord =
                key_event_to_chord(&ev(None, &ch.to_string(), BridgeModifiers::default())).unwrap();
            assert_eq!(chord.key, Key::Char(ch));
        }
    }

    #[test]
    fn modifiers_map_across_the_board() {
        let mods = BridgeModifiers {
            ctrl: true,
            alt: true,
            logo: true,
            caps_lock: true,
            num_lock: true,
            ..Default::default()
        };
        let chord = key_event_to_chord(&ev(None, "Tab", mods)).unwrap();
        assert_eq!(
            chord.modifiers,
            Modifiers::CTRL | Modifiers::ALT | Modifiers::SUPER,
            "caps/num lock are toggles, never chord modifiers"
        );
    }

    #[test]
    fn releases_and_repeats_drop() {
        let released = KeyEvent::new(
            ScanCode(0),
            KeyCode::Named("j".into()),
            BridgeModifiers::default(),
            KeyState::Released,
            Some("j".into()),
            false,
        );
        assert!(key_event_to_chord(&released).is_none());
        assert!(key_event_to_chord_with_repeat(&released).is_none());

        let repeat = KeyEvent::new(
            ScanCode(0),
            KeyCode::Named("j".into()),
            BridgeModifiers::default(),
            KeyState::Pressed,
            Some("j".into()),
            true,
        );
        assert!(key_event_to_chord(&repeat).is_none());
        // …but the text-input path accepts auto-repeat.
        assert!(key_event_to_chord_with_repeat(&repeat).is_some());
    }

    #[test]
    fn unmapped_and_multi_codepoint_drop() {
        // Multi-char symbolic name outside the table.
        assert!(key_event_to_chord(&ev(None, "Caps_Lock", BridgeModifiers::default())).is_none());
        // Dead-key composition text — IME path's job.
        assert!(
            key_event_to_chord(&ev(Some("a\u{0302}"), "a", BridgeModifiers::default())).is_none()
        );
        // Raw keysym with no text at all.
        let sym = KeyEvent::new(
            ScanCode(0),
            KeyCode::Sym(0x1008ff11),
            BridgeModifiers::default(),
            KeyState::Pressed,
            None,
            false,
        );
        assert!(key_event_to_chord(&sym).is_none());
    }
}
