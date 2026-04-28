//! winit `KeyEvent` → modal [`KeyChord`] adapter.
//!
//! Gated behind the `winit` Cargo feature so the modal engine itself
//! stays winit-agnostic — only the apps wiring the engine into a winit
//! event loop need to pull this in.
//!
//! # Mapping rules
//!
//! - Only `ElementState::Pressed` events produce a chord. Releases and
//!   key-repeat events return `None` for now (Phase 2 mirrors vim's
//!   default behaviour where holding `j` doesn't auto-repeat scrolls).
//! - `winit::keyboard::Key::Character(s)` maps to [`Key::Char`] when
//!   `s.chars().count() == 1`. Multi-codepoint sequences (dead-key
//!   composition, IMEs producing strings like "â" via two events) drop
//!   for now — Phase 2 only routes single-codepoint chords. We will
//!   revisit when implementing edit-mode IME support.
//! - `winit::keyboard::NamedKey::*` cases fold into [`NamedKey`] for
//!   the keys vim notation has dedicated names for. Anything else
//!   returns `None`.
//! - Modifiers come from a separately-tracked
//!   [`winit::keyboard::ModifiersState`]: winit 0.30 split modifier
//!   tracking out of the per-event payload, so callers receive
//!   `WindowEvent::ModifiersChanged` and stash the latest state, then
//!   pass it alongside each `KeyEvent`.
//!
//! # Shift quirk
//!
//! winit's `logical_key` already incorporates shift into the produced
//! character — pressing Shift+`j` yields `Character("J")`, not
//! `Character("j")` plus a shift modifier. Our [`Modifiers::SHIFT`]
//! still gets set if the modifier state has shift held, so a binding
//! like `<S-j>` (which the parser normalises to `Char('J')` plus
//! SHIFT) lines up. Bare uppercase ASCII letters in keymap tables
//! also include SHIFT (see `parse_char` in `key.rs`), keeping
//! lookups consistent.

use crate::key::{Key, KeyChord, Modifiers, NamedKey};
use winit::event::{ElementState, KeyEvent};
use winit::keyboard::{Key as WKey, ModifiersState, NamedKey as WNamed};

/// Convert a winit `KeyEvent` + tracked modifier state into a
/// [`KeyChord`]. Returns `None` for releases, repeats, and anything
/// that doesn't correspond to a chord we route through the trie
/// (multi-codepoint character strings, modifier-only presses, dead
/// keys, etc.).
pub fn key_event_to_chord(event: &KeyEvent, modifiers: ModifiersState) -> Option<KeyChord> {
    if event.state != ElementState::Pressed {
        return None;
    }
    if event.repeat {
        return None;
    }
    chord_from_logical(&event.logical_key, modifiers)
}

/// Like [`key_event_to_chord`] but accepts auto-repeat events. Used by
/// text-input surfaces (omnibar, command line) where holding backspace
/// or a character key should fire continuously.
pub fn key_event_to_chord_with_repeat(
    event: &KeyEvent,
    modifiers: ModifiersState,
) -> Option<KeyChord> {
    if event.state != ElementState::Pressed {
        return None;
    }
    chord_from_logical(&event.logical_key, modifiers)
}

fn chord_from_logical(logical: &WKey, modifiers: ModifiersState) -> Option<KeyChord> {
    let mods = modifiers_to_internal(modifiers);
    match logical {
        WKey::Character(s) => {
            let mut chars = s.chars();
            let first = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            Some(KeyChord {
                modifiers: mods,
                key: Key::Char(first),
            })
        }
        WKey::Named(named) => {
            let mapped = map_named(*named)?;
            Some(KeyChord {
                modifiers: mods,
                key: Key::Named(mapped),
            })
        }
        _ => None,
    }
}

fn modifiers_to_internal(m: ModifiersState) -> Modifiers {
    let mut out = Modifiers::empty();
    if m.shift_key() {
        out |= Modifiers::SHIFT;
    }
    if m.control_key() {
        out |= Modifiers::CTRL;
    }
    if m.alt_key() {
        out |= Modifiers::ALT;
    }
    if m.super_key() {
        out |= Modifiers::SUPER;
    }
    out
}

fn map_named(n: WNamed) -> Option<NamedKey> {
    Some(match n {
        WNamed::Escape => NamedKey::Esc,
        WNamed::Enter => NamedKey::CR,
        WNamed::Tab => NamedKey::Tab,
        WNamed::Backspace => NamedKey::BS,
        WNamed::Space => NamedKey::Space,
        WNamed::ArrowUp => NamedKey::Up,
        WNamed::ArrowDown => NamedKey::Down,
        WNamed::ArrowLeft => NamedKey::Left,
        WNamed::ArrowRight => NamedKey::Right,
        WNamed::Home => NamedKey::Home,
        WNamed::End => NamedKey::End,
        WNamed::PageUp => NamedKey::PageUp,
        WNamed::PageDown => NamedKey::PageDown,
        WNamed::Insert => NamedKey::Insert,
        WNamed::Delete => NamedKey::Delete,
        WNamed::F1 => NamedKey::F(1),
        WNamed::F2 => NamedKey::F(2),
        WNamed::F3 => NamedKey::F(3),
        WNamed::F4 => NamedKey::F(4),
        WNamed::F5 => NamedKey::F(5),
        WNamed::F6 => NamedKey::F(6),
        WNamed::F7 => NamedKey::F(7),
        WNamed::F8 => NamedKey::F(8),
        WNamed::F9 => NamedKey::F(9),
        WNamed::F10 => NamedKey::F(10),
        WNamed::F11 => NamedKey::F(11),
        WNamed::F12 => NamedKey::F(12),
        _ => return None,
    })
}

/// Internal helper exposed for testing: do the logical_key →
/// (Key/None) translation step, given an already-decided pressed-and-
/// not-repeating event. The public [`key_event_to_chord`] also gates
/// on `state` and `repeat`; this helper is the rest of the work.
///
/// We test through this seam because winit's `KeyEvent` has a
/// `pub(crate)` `platform_specific` field, so we can't synthesize one
/// directly in our unit tests without a real platform backend.
#[cfg(test)]
fn translate_key_test_only(logical_key: &WKey, modifiers: ModifiersState) -> Option<KeyChord> {
    let mods = modifiers_to_internal(modifiers);
    match logical_key {
        WKey::Character(s) => {
            let mut chars = s.chars();
            let first = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            Some(KeyChord {
                modifiers: mods,
                key: Key::Char(first),
            })
        }
        WKey::Named(named) => {
            let mapped = map_named(*named)?;
            Some(KeyChord {
                modifiers: mods,
                key: Key::Named(mapped),
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::keyboard::SmolStr;

    fn translate(k: WKey, m: ModifiersState) -> Option<KeyChord> {
        translate_key_test_only(&k, m)
    }

    #[test]
    fn plain_j_is_char_j_no_modifiers() {
        let chord =
            translate(WKey::Character(SmolStr::new("j")), ModifiersState::empty()).expect("some");
        assert_eq!(chord.key, Key::Char('j'));
        assert!(chord.modifiers.is_empty());
    }

    #[test]
    fn shift_j_carries_uppercase_and_shift_flag() {
        // winit quirk: `logical_key` is post-shift, so Shift+j yields
        // `Character("J")` (capital). We additionally set the SHIFT
        // modifier bit because the user-supplied `ModifiersState` has
        // shift held.
        let chord =
            translate(WKey::Character(SmolStr::new("J")), ModifiersState::SHIFT).expect("some");
        assert_eq!(chord.key, Key::Char('J'));
        assert!(chord.modifiers.contains(Modifiers::SHIFT));
    }

    #[test]
    fn ctrl_w_includes_ctrl_modifier() {
        let chord =
            translate(WKey::Character(SmolStr::new("w")), ModifiersState::CONTROL).expect("some");
        assert!(chord.modifiers.contains(Modifiers::CTRL));
        assert_eq!(chord.key, Key::Char('w'));
    }

    #[test]
    fn escape_named_key() {
        let chord = translate(WKey::Named(WNamed::Escape), ModifiersState::empty()).expect("some");
        assert_eq!(chord.key, Key::Named(NamedKey::Esc));
    }

    #[test]
    fn arrow_keys_map() {
        for (named, expect) in [
            (WNamed::ArrowUp, NamedKey::Up),
            (WNamed::ArrowDown, NamedKey::Down),
            (WNamed::ArrowLeft, NamedKey::Left),
            (WNamed::ArrowRight, NamedKey::Right),
        ] {
            let chord = translate(WKey::Named(named), ModifiersState::empty()).unwrap();
            assert_eq!(chord.key, Key::Named(expect));
        }
    }

    #[test]
    fn function_keys_f1_through_f12() {
        let pairs = [
            (WNamed::F1, 1u8),
            (WNamed::F2, 2),
            (WNamed::F3, 3),
            (WNamed::F4, 4),
            (WNamed::F5, 5),
            (WNamed::F6, 6),
            (WNamed::F7, 7),
            (WNamed::F8, 8),
            (WNamed::F9, 9),
            (WNamed::F10, 10),
            (WNamed::F11, 11),
            (WNamed::F12, 12),
        ];
        for (named, n) in pairs {
            let chord = translate(WKey::Named(named), ModifiersState::empty()).unwrap();
            assert_eq!(chord.key, Key::Named(NamedKey::F(n)));
        }
    }

    #[test]
    fn multi_codepoint_character_drops() {
        // "a + combining circumflex" — two scalar values in one
        // SmolStr.
        let chord = translate(
            WKey::Character(SmolStr::new("a\u{0302}")),
            ModifiersState::empty(),
        );
        assert!(chord.is_none());
    }

    #[test]
    fn unmapped_named_returns_none() {
        // `CapsLock` isn't in our `NamedKey` mapping.
        let chord = translate(WKey::Named(WNamed::CapsLock), ModifiersState::empty());
        assert!(chord.is_none());
    }

    #[test]
    fn ctrl_modifier_combined_with_named() {
        let chord = translate(
            WKey::Named(WNamed::Tab),
            ModifiersState::CONTROL | ModifiersState::SHIFT,
        )
        .unwrap();
        assert_eq!(chord.key, Key::Named(NamedKey::Tab));
        assert!(chord.modifiers.contains(Modifiers::CTRL));
        assert!(chord.modifiers.contains(Modifiers::SHIFT));
    }
}
