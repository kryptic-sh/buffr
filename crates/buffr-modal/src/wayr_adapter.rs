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
//!   A named key that is itself a single printable codepoint (some
//!   sources report the digit row that way) falls back to a `Char`
//!   chord, and `"space"` / `" "` land on `Char(' ')` so a
//!   `leader = ' '` binding matches.
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

use crate::adapter::chord_from_parts;
use crate::key::{KeyChord, Modifiers};
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
    // Both inputs go to the shared translator: prefer the
    // layout-composed text (matches the winit adapter's `logical_key`
    // behaviour where Shift+j arrives as "J"; wayr ≥ 0.1.2 filters
    // ASCII control characters out at the source), falling back to
    // the xkb keysym name for the control-key family (Return,
    // BackSpace, Tab, Escape, Delete, …).
    let named = match &event.key_code {
        WKey::Named(name) => Some(name.as_str()),
        _ => None,
    };
    chord_from_parts(
        event.text.as_deref(),
        named,
        modifiers_to_internal(event.modifiers),
    )
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key::{Key, NamedKey};
    use wayr::Modifiers as WMods;

    /// Test seam: `wayr::KeyEvent` is `#[non_exhaustive]` and can't be
    /// built outside the wayr crate, so tests drive the *production*
    /// translator [`chord_from_parts`] with the same three inputs
    /// [`chord_from_event`] extracts from an event. Only the
    /// state / repeat gating of the public entry points is skipped.
    ///
    /// wayr ≥ 0.1.2 strips ASCII control text at the source, so the
    /// regression tests below pass `text = None` for Return /
    /// BackSpace / Tab / Escape / Delete and exercise the named-key
    /// fallback path.
    fn mk(text: Option<&str>, named: Option<&str>, mods: WMods) -> Option<KeyChord> {
        chord_from_parts(text, named, modifiers_to_internal(mods))
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

    // The control-key family (Return / BackSpace / Tab / Escape /
    // Delete) reaches the adapter as `text = None` + `key_code =
    // Named("<keysym>")` because wayr ≥ 0.1.2 strips ASCII control
    // characters from `text` at the source. These tests assert the
    // named-key fallback path resolves the right `NamedKey` variant.

    #[test]
    fn enter_named_only_lands_on_named_cr() {
        let chord = mk(None, Some("Return"), WMods::default()).expect("some");
        assert_eq!(chord.key, Key::Named(NamedKey::CR));
    }

    #[test]
    fn backspace_named_only_lands_on_named_bs() {
        let chord = mk(None, Some("BackSpace"), WMods::default()).expect("some");
        assert_eq!(chord.key, Key::Named(NamedKey::BS));
    }

    #[test]
    fn tab_named_only_lands_on_named_tab() {
        let chord = mk(None, Some("Tab"), WMods::default()).expect("some");
        assert_eq!(chord.key, Key::Named(NamedKey::Tab));
    }

    // (Escape already covered by `escape_named_key_no_text` above.)

    #[test]
    fn delete_named_only_lands_on_named_delete() {
        let chord = mk(None, Some("Delete"), WMods::default()).expect("some");
        assert_eq!(chord.key, Key::Named(NamedKey::Delete));
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
