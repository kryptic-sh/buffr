//! wayr `KeyEvent` → modal [`KeyChord`] adapter.
//!
//! Gated behind the `wayr` Cargo feature, parallel to the `winit`
//! adapter. The mapping is intentionally identical in semantics; only
//! the source event shape differs.
//!
//! # Mapping rules
//!
//! - Only [`wayr::KeyState::Pressed`] events produce a chord. Releases
//!   and key-repeat events return `None` (parity with the winit adapter
//!   so vim-style hold-to-repeat doesn't auto-fire scrolls).
//! - When [`wayr::KeyEvent::text`] is a single Unicode scalar, the
//!   chord is `Key::Char(<that scalar>)`. wayr's xkbcommon pipeline
//!   already applies modifier composition, so Shift+`j` arrives as
//!   `text = Some("J")` — same shape winit reports. Multi-codepoint
//!   `text` (dead-key composition, IMEs producing "â" via two events)
//!   drops for now; the IME path will own that case.
//! - When `text` is absent we look at [`wayr::KeyCode::Named`] — wayr
//!   surfaces xkbcommon's symbolic name (`"Escape"`, `"Return"`,
//!   `"Tab"`, `"BackSpace"`, `"Up"`, `"F1"`, ...). We translate the
//!   named keys vim notation has dedicated names for.
//! - [`wayr::Modifiers`] maps straight to our internal [`Modifiers`].
//!   wayr's `logo` field becomes `SUPER`; `caps_lock` / `num_lock`
//!   stay out of the chord (they're toggles, not modifiers in the
//!   vim sense).
//!
//! # Shift quirk (same as winit)
//!
//! xkbcommon's `text` already bakes shift into the produced character
//! (`Shift+j` → `"J"`), but we still set [`Modifiers::SHIFT`] when
//! the modifier state has shift held. This matches the winit adapter
//! so the same keymap tables work under both backends.

use crate::key::{Key, KeyChord, Modifiers, NamedKey};
use wayr::{KeyCode as WKey, KeyEvent, KeyState, Modifiers as WMods};

/// Convert a wayr `KeyEvent` into a [`KeyChord`]. Returns `None` for
/// releases, repeats, and anything we don't route through the trie
/// (multi-codepoint text, modifier-only presses, unmapped xkb keysyms).
pub fn key_event_to_chord(event: &KeyEvent) -> Option<KeyChord> {
    if event.state != KeyState::Pressed {
        return None;
    }
    if event.repeat {
        return None;
    }
    chord_from_event(event)
}

/// Like [`key_event_to_chord`] but accepts auto-repeat events. Used by
/// text-input surfaces (omnibar, command line) where holding backspace
/// or a character key should fire continuously.
pub fn key_event_to_chord_with_repeat(event: &KeyEvent) -> Option<KeyChord> {
    if event.state != KeyState::Pressed {
        return None;
    }
    chord_from_event(event)
}

fn chord_from_event(event: &KeyEvent) -> Option<KeyChord> {
    let mods = modifiers_to_internal(event.modifiers);

    // Prefer the layout-composed text — matches the winit adapter's
    // "logical_key" behaviour where Shift+j arrives as "J".
    if let Some(text) = event.text.as_deref()
        && !text.is_empty()
    {
        let mut chars = text.chars();
        let first = chars.next()?;
        if chars.next().is_some() {
            // Multi-codepoint composition — IME path's job.
            return None;
        }
        let mut effective = mods;
        let mut ch = first;
        // Drop SHIFT when the layout has already baked it into a
        // non-alphabetic glyph (`+` from Shift+=, `!` from Shift+1).
        // Alphabetic stays untouched so `Shift+a` → `(SHIFT, 'A')`
        // matches the parser's canonical form.
        if effective.contains(Modifiers::SHIFT) && first.is_ascii() && !first.is_ascii_alphabetic()
        {
            effective.remove(Modifiers::SHIFT);
        }
        // Ctrl+letter is case-insensitive in the parser
        // (`<C-h>` and `<C-H>` both produce `(CTRL, 'h')`), so
        // normalize alphabetic chars to lowercase under CTRL.
        if effective.contains(Modifiers::CTRL) && ch.is_ascii_alphabetic() {
            ch = ch.to_ascii_lowercase();
        }
        return Some(KeyChord {
            modifiers: effective,
            key: Key::Char(ch),
        });
    }

    // Fall through to the named-key path when `text` is absent (Escape,
    // arrows, function keys, etc.).
    if let WKey::Named(name) = &event.key_code {
        // Space comes through as text " " in practice — this branch
        // catches the rare "space without text" edge case.
        if name == "space" {
            return Some(KeyChord {
                modifiers: mods,
                key: Key::Char(' '),
            });
        }
        let mapped = map_named(name)?;
        return Some(KeyChord {
            modifiers: mods,
            key: Key::Named(mapped),
        });
    }
    None
}

fn modifiers_to_internal(m: WMods) -> Modifiers {
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

/// Map an xkbcommon keysym name to our [`NamedKey`]. The names come
/// from `/usr/include/X11/keysymdef.h` (the canonical xkb keysym
/// table). Anything outside the vim-notation set returns `None`.
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

/// Internal testing seam: do the chord translation without
/// constructing a full [`wayr::KeyEvent`] (which is `#[non_exhaustive]`
/// and can't be built outside the wayr crate). Mirrors the
/// `translate_key_test_only` helper in `winit_adapter`.
///
/// Reproduces the exact branching of [`chord_from_event`] modulo the
/// state / repeat gating that the public entry points enforce.
#[cfg(test)]
fn translate_test_only(
    text: Option<&str>,
    named: Option<&str>,
    modifiers: WMods,
) -> Option<KeyChord> {
    let mods = modifiers_to_internal(modifiers);
    if let Some(text) = text
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
    if let Some(name) = named {
        if name == "space" {
            return Some(KeyChord {
                modifiers: mods,
                key: Key::Char(' '),
            });
        }
        let mapped = map_named(name)?;
        return Some(KeyChord {
            modifiers: mods,
            key: Key::Named(mapped),
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use wayr::Modifiers as WMods;

    fn mk(text: Option<&str>, named: Option<&str>, mods: WMods) -> Option<KeyChord> {
        translate_test_only(text, named, mods)
    }

    #[test]
    fn plain_j_via_text() {
        let chord = mk(Some("j"), Some("j"), WMods::default()).expect("some");
        assert_eq!(chord.key, Key::Char('j'));
        assert!(chord.modifiers.is_empty());
    }

    #[test]
    fn shift_j_carries_uppercase_and_shift_flag() {
        let chord = mk(
            Some("J"),
            Some("J"),
            WMods {
                shift: true,
                ..Default::default()
            },
        )
        .expect("some");
        assert_eq!(chord.key, Key::Char('J'));
        assert!(chord.modifiers.contains(Modifiers::SHIFT));
    }

    #[test]
    fn ctrl_w_includes_ctrl_modifier() {
        let chord = mk(
            Some("w"),
            Some("w"),
            WMods {
                ctrl: true,
                ..Default::default()
            },
        )
        .expect("some");
        assert!(chord.modifiers.contains(Modifiers::CTRL));
        assert_eq!(chord.key, Key::Char('w'));
    }

    #[test]
    fn ctrl_shift_h_normalizes_to_lowercase() {
        // Mirrors the winit adapter: Ctrl+Shift+H should land on
        // `(CTRL|SHIFT, 'h')` so the parser's `<C-S-h>` form matches.
        let chord = mk(
            Some("H"),
            Some("H"),
            WMods {
                ctrl: true,
                shift: true,
                ..Default::default()
            },
        )
        .expect("some");
        assert_eq!(chord.key, Key::Char('h'));
        assert!(chord.modifiers.contains(Modifiers::CTRL));
        assert!(chord.modifiers.contains(Modifiers::SHIFT));
    }

    #[test]
    fn shift_plus_drops_shift_modifier() {
        // `+` is the shifted form of `=` on US — the parser writes
        // `+` directly without `<S->`, so the adapter must shed SHIFT.
        let chord = mk(
            Some("+"),
            Some("plus"),
            WMods {
                shift: true,
                ..Default::default()
            },
        )
        .expect("some");
        assert_eq!(chord.key, Key::Char('+'));
        assert!(!chord.modifiers.contains(Modifiers::SHIFT));
    }

    #[test]
    fn escape_named_key_no_text() {
        let chord = mk(None, Some("Escape"), WMods::default()).expect("some");
        assert_eq!(chord.key, Key::Named(NamedKey::Esc));
    }

    #[test]
    fn arrow_keys_map() {
        for (xkb, expect) in [
            ("Up", NamedKey::Up),
            ("Down", NamedKey::Down),
            ("Left", NamedKey::Left),
            ("Right", NamedKey::Right),
        ] {
            let chord = mk(None, Some(xkb), WMods::default()).unwrap();
            assert_eq!(chord.key, Key::Named(expect));
        }
    }

    #[test]
    fn function_keys_f1_through_f12() {
        for n in 1u8..=12 {
            let chord = mk(None, Some(&format!("F{n}")), WMods::default()).unwrap();
            assert_eq!(chord.key, Key::Named(NamedKey::F(n)));
        }
    }

    #[test]
    fn page_up_via_xkb_prior() {
        // xkbcommon emits PageUp as "Prior" (and PageDown as "Next").
        let chord = mk(None, Some("Prior"), WMods::default()).unwrap();
        assert_eq!(chord.key, Key::Named(NamedKey::PageUp));
    }

    #[test]
    fn multi_codepoint_text_drops() {
        // "a + combining circumflex" — two scalars in one text.
        assert!(mk(Some("a\u{0302}"), None, WMods::default()).is_none());
    }

    #[test]
    fn unmapped_named_returns_none() {
        assert!(mk(None, Some("Caps_Lock"), WMods::default()).is_none());
    }

    #[test]
    fn ctrl_shift_tab_combined() {
        let chord = mk(
            None,
            Some("Tab"),
            WMods {
                ctrl: true,
                shift: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(chord.key, Key::Named(NamedKey::Tab));
        assert!(chord.modifiers.contains(Modifiers::CTRL));
        assert!(chord.modifiers.contains(Modifiers::SHIFT));
    }
}
