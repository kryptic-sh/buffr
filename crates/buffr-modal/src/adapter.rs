//! Translation helpers shared by the platform key adapters.
//!
//! The winit, wayr and bridge adapters all end up doing the same two
//! jobs once their source-specific event shape has been peeled away:
//!
//! 1. Normalise a single composed codepoint plus the held modifier
//!    state into a [`KeyChord`] ([`char_chord`]).
//! 2. Resolve a symbolic key name into a `NamedKey` (`map_named`,
//!    xkbcommon-shaped adapters only).
//!
//! Keeping one copy is what stops the adapters drifting apart — they
//! previously carried three hand-copies of the same code, two of
//! which had already lost normalisation steps.

#[cfg(any(feature = "wayr", feature = "bridge"))]
use crate::key::NamedKey;
use crate::key::{Key, KeyChord, Modifiers};

/// Build a `Char` chord from one composed codepoint and the held
/// modifiers, applying the two normalisations every adapter needs:
///
/// - **SHIFT drop.** When the layout has already baked shift into a
///   non-alphabetic ASCII glyph (`+` from `Shift+=`, `!` from
///   `Shift+1`) the SHIFT bit is removed — the keymap parser writes
///   those bindings as the bare glyph. Alphabetic keeps SHIFT so
///   `Shift+a` → `(SHIFT, 'A')` matches the parser's canonical form.
/// - **CTRL lowercase fold.** `<C-h>` and `<C-H>` both parse to
///   `(CTRL, 'h')`, so uppercase letters fold to lowercase whenever
///   CTRL is held. Without this, `<C-S-h>` (parsed as
///   `(CTRL|SHIFT, 'h')`) never matched the adapter's
///   `(CTRL|SHIFT, 'H')`.
pub(crate) fn char_chord(ch: char, mods: Modifiers) -> KeyChord {
    let mut modifiers = mods;
    let mut key = ch;
    if modifiers.contains(Modifiers::SHIFT) && ch.is_ascii() && !ch.is_ascii_alphabetic() {
        modifiers.remove(Modifiers::SHIFT);
    }
    if modifiers.contains(Modifiers::CTRL) && key.is_ascii_alphabetic() {
        key = key.to_ascii_lowercase();
    }
    KeyChord {
        modifiers,
        key: Key::Char(key),
    }
}

/// Map an xkbcommon keysym name to our [`NamedKey`]. The names come
/// from `/usr/include/X11/keysymdef.h` (the canonical xkb keysym
/// table). Anything outside the vim-notation set returns `None`.
#[cfg(any(feature = "wayr", feature = "bridge"))]
pub(crate) fn map_named(name: &str) -> Option<NamedKey> {
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

/// Chord translation for the xkbcommon-shaped adapters (wayr and the
/// toolkit-agnostic bridge). Both hand us the same three inputs, so
/// they share one implementation:
///
/// - `text` — the layout-composed text, if the source produced any.
///   A single scalar becomes a `Char` chord; multi-codepoint
///   compositions (dead keys, IMEs) drop — that's the IME path's job.
/// - `named` — the symbolic key name, consulted when `text` is
///   absent. Covers the control-key family (Return / BackSpace / Tab
///   / Escape / Delete …) that xkb sources filter out of `text`.
/// - `mods` — the already-converted modifier state.
///
/// Two fallbacks live here on purpose:
///
/// - `"space"` / `" "` resolve to `Char(' ')` rather than
///   `Named(Space)` so a `leader = ' '` binding (the default) matches
///   the canonical form the keymap parser emits.
/// - A `named` value that is itself a single printable codepoint is
///   treated as that character. winit's bridge stuffs
///   `logical_key.Character("0")` into `KeyCode::Named("0")` (the
///   bridge `KeyCode` enum has no `Char` variant), so without this
///   the digit / punctuation rows (`0`, `-`, `=`) would never match.
#[cfg(any(feature = "wayr", feature = "bridge"))]
pub(crate) fn chord_from_parts(
    text: Option<&str>,
    named: Option<&str>,
    mods: Modifiers,
) -> Option<KeyChord> {
    if let Some(text) = text
        && !text.is_empty()
    {
        let mut chars = text.chars();
        let first = chars.next()?;
        if chars.next().is_some() {
            // Multi-codepoint composition — IME path's job.
            return None;
        }
        return Some(char_chord(first, mods));
    }

    let name = named?;
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
    let mut chars = name.chars();
    if let (Some(first), None) = (chars.next(), chars.next())
        && !first.is_ascii_control()
    {
        return Some(char_chord(first, mods));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_char_keeps_modifiers() {
        let chord = char_chord('j', Modifiers::empty());
        assert_eq!(chord.key, Key::Char('j'));
        assert!(chord.modifiers.is_empty());
    }

    #[test]
    fn shift_alpha_keeps_shift() {
        let chord = char_chord('J', Modifiers::SHIFT);
        assert_eq!(chord.key, Key::Char('J'));
        assert!(chord.modifiers.contains(Modifiers::SHIFT));
    }

    #[test]
    fn shift_non_alpha_drops_shift() {
        for glyph in ['+', '!', '?'] {
            let chord = char_chord(glyph, Modifiers::SHIFT);
            assert_eq!(chord.key, Key::Char(glyph));
            assert!(
                !chord.modifiers.contains(Modifiers::SHIFT),
                "{glyph} must shed SHIFT"
            );
        }
    }

    #[test]
    fn non_ascii_shifted_glyph_keeps_shift() {
        // Only ASCII gets the shed — non-ASCII layouts are left alone.
        let chord = char_chord('Ä', Modifiers::SHIFT);
        assert_eq!(chord.key, Key::Char('Ä'));
        assert!(chord.modifiers.contains(Modifiers::SHIFT));
    }

    #[test]
    fn ctrl_folds_uppercase_to_lowercase() {
        let chord = char_chord('H', Modifiers::CTRL | Modifiers::SHIFT);
        assert_eq!(chord.key, Key::Char('h'));
        assert!(chord.modifiers.contains(Modifiers::CTRL));
        assert!(chord.modifiers.contains(Modifiers::SHIFT));
    }
}
