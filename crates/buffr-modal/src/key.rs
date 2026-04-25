//! Vim-notation key parser.
//!
//! Parses strings like `<C-w>v`, `gT`, `<leader>fa`, `<C-S-Tab>` into
//! a list of [`KeyChord`]s the page-mode dispatcher can match against.
//!
//! Notation supported:
//!
//! - `<C-...>` — Ctrl
//! - `<S-...>` — Shift (also implied by uppercase ASCII letters when
//!   bare)
//! - `<M-...>` / `<A-...>` — Alt / Meta
//! - `<D-...>` — Super (Cmd on macOS)
//! - `<leader>` — abstract placeholder; resolution to a concrete char
//!   happens at trie-build time, not here.
//! - `<Space>`, `<Esc>`, `<CR>` / `<Enter>`, `<BS>` / `<Backspace>`,
//!   `<Tab>`, `<S-Tab>` / `<BackTab>`, `<Up>`, `<Down>`, `<Left>`,
//!   `<Right>`, `<Home>`, `<End>`, `<PageUp>`, `<PageDown>`,
//!   `<Insert>`, `<Delete>`, `<F1>`–`<F12>`
//! - Bare characters, including punctuation: `g`, `T`, `,`, `;`, `:`,
//!   `/`, `?`, `<` (escaped as `<lt>`).
//!
//! Whitespace in the input is ignored. Empty input yields an empty
//! `Vec`. Embedded literals like `Hello` parse as five separate
//! chords (one per char) — this parser is *not* a string-mode parser.

use bitflags::bitflags;
use std::fmt;

bitflags! {
    /// Modifier set for one [`KeyChord`].
    ///
    /// Vim notation maps as: `C-` → [`Modifiers::CTRL`], `S-` →
    /// [`Modifiers::SHIFT`], `M-`/`A-` → [`Modifiers::ALT`], `D-` →
    /// [`Modifiers::SUPER`].
    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct Modifiers: u8 {
        const CTRL  = 0b0001;
        const SHIFT = 0b0010;
        const ALT   = 0b0100;
        const SUPER = 0b1000;
    }
}

/// One position in a chord sequence.
///
/// Field order is `modifiers` then `key` so debug output reads as
/// `KeyChord { modifiers: CTRL, key: Char('w') }` — matches how vim
/// docs render these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyChord {
    pub modifiers: Modifiers,
    pub key: Key,
}

impl KeyChord {
    pub const fn new(modifiers: Modifiers, key: Key) -> Self {
        Self { modifiers, key }
    }

    pub const fn plain(key: Key) -> Self {
        Self {
            modifiers: Modifiers::empty(),
            key,
        }
    }

    pub const fn char(c: char) -> Self {
        Self::plain(Key::Char(c))
    }
}

/// What was pressed.
///
/// `Char` carries the printable codepoint; `Named(NamedKey)` covers
/// the discrete keys vim notation has dedicated names for. `Leader`
/// is abstract and gets resolved by the keymap when bindings are
/// inserted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    Char(char),
    Named(NamedKey),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NamedKey {
    Esc,
    /// `<CR>` / `<Enter>` / `<Return>`.
    CR,
    Tab,
    /// `<S-Tab>` / `<BackTab>`. Distinct atom because some terminals
    /// emit it as its own keysym rather than Shift+Tab.
    BackTab,
    BS,
    Space,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    F(u8),
    /// Abstract leader placeholder. The keymap resolves this to a
    /// concrete `Char` at bind time using its configured leader.
    Leader,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("unknown key name <{0}>")]
    UnknownNamed(String),
    #[error("unclosed `<` in key notation")]
    UnclosedBracket,
    #[error("empty `<...>` block")]
    EmptyBracket,
    #[error("modifier `<{0}->` with no following key")]
    DanglingModifier(String),
    #[error("expected exactly one chord, got {0}")]
    ExpectedOneChord(usize),
}

/// Parse `input` into a sequence of [`KeyChord`]s.
pub fn parse_keys(input: &str) -> Result<Vec<KeyChord>, ParseError> {
    let mut out = Vec::new();
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        // Drop ASCII whitespace between chords; configs may format
        // bindings with spaces for readability (`<C-w> v`).
        if c.is_ascii_whitespace() {
            continue;
        }
        if c == '<' {
            let mut name = String::new();
            let mut closed = false;
            for nc in chars.by_ref() {
                if nc == '>' {
                    closed = true;
                    break;
                }
                name.push(nc);
            }
            if !closed {
                return Err(ParseError::UnclosedBracket);
            }
            if name.is_empty() {
                return Err(ParseError::EmptyBracket);
            }
            out.push(parse_named(&name)?);
        } else {
            out.push(parse_char(c));
        }
    }
    Ok(out)
}

/// Parse exactly one chord. Errors if the input parses to zero chords
/// or more than one.
pub fn parse_key(input: &str) -> Result<KeyChord, ParseError> {
    let chords = parse_keys(input)?;
    if chords.len() != 1 {
        return Err(ParseError::ExpectedOneChord(chords.len()));
    }
    Ok(chords.into_iter().next().expect("len checked above"))
}

fn parse_char(c: char) -> KeyChord {
    let mut mods = Modifiers::empty();
    if c.is_ascii_uppercase() {
        mods |= Modifiers::SHIFT;
    }
    KeyChord {
        modifiers: mods,
        key: Key::Char(c),
    }
}

fn parse_named(raw: &str) -> Result<KeyChord, ParseError> {
    // Special: `<lt>` is the literal `<`.
    if raw.eq_ignore_ascii_case("lt") {
        return Ok(KeyChord::char('<'));
    }

    let (mods, tail) = parse_modifiers(raw);

    if tail.is_empty() {
        return Err(ParseError::DanglingModifier(raw.to_string()));
    }

    // Resolve the tail.
    let key = if tail.eq_ignore_ascii_case("leader") {
        Key::Named(NamedKey::Leader)
    } else if tail.chars().count() == 1 {
        // `chars().count() == 1` so we don't slice on a multi-byte
        // char by accident.
        let ch = tail.chars().next().expect("len checked above");
        // Ctrl+letter is case-insensitive; preserve case otherwise so
        // `<S-a>` and `<S-A>` parse the same.
        let ch = if mods.contains(Modifiers::CTRL) {
            ch.to_ascii_lowercase()
        } else if mods.contains(Modifiers::SHIFT) && ch.is_ascii_alphabetic() {
            ch.to_ascii_uppercase()
        } else {
            ch
        };
        Key::Char(ch)
    } else {
        Key::Named(parse_named_key(tail)?)
    };

    Ok(KeyChord {
        modifiers: mods,
        key,
    })
}

/// Strip leading `C-` / `S-` / `M-` / `A-` / `D-` prefixes (ASCII
/// case-insensitive) and return the remaining tail.
fn parse_modifiers(raw: &str) -> (Modifiers, &str) {
    let mut mods = Modifiers::empty();
    let mut tail = raw;
    loop {
        let lower_prefix = tail.get(..2).map(str::to_ascii_lowercase);
        match lower_prefix.as_deref() {
            Some("c-") => {
                mods |= Modifiers::CTRL;
                tail = &tail[2..];
            }
            Some("s-") => {
                mods |= Modifiers::SHIFT;
                tail = &tail[2..];
            }
            Some("m-") | Some("a-") => {
                mods |= Modifiers::ALT;
                tail = &tail[2..];
            }
            Some("d-") => {
                mods |= Modifiers::SUPER;
                tail = &tail[2..];
            }
            _ => return (mods, tail),
        }
    }
}

fn parse_named_key(name: &str) -> Result<NamedKey, ParseError> {
    let n = name.to_ascii_lowercase();
    Ok(match n.as_str() {
        "esc" | "escape" => NamedKey::Esc,
        "cr" | "enter" | "return" => NamedKey::CR,
        "bs" | "backspace" => NamedKey::BS,
        "tab" => NamedKey::Tab,
        "backtab" => NamedKey::BackTab,
        "space" => NamedKey::Space,
        "up" => NamedKey::Up,
        "down" => NamedKey::Down,
        "left" => NamedKey::Left,
        "right" => NamedKey::Right,
        "home" => NamedKey::Home,
        "end" => NamedKey::End,
        "pageup" | "pgup" => NamedKey::PageUp,
        "pagedown" | "pgdn" => NamedKey::PageDown,
        "insert" | "ins" => NamedKey::Insert,
        "delete" | "del" => NamedKey::Delete,
        s if s.starts_with('f') => {
            let num: u8 = s[1..]
                .parse()
                .map_err(|_| ParseError::UnknownNamed(name.to_string()))?;
            if !(1..=12).contains(&num) {
                return Err(ParseError::UnknownNamed(name.to_string()));
            }
            NamedKey::F(num)
        }
        _ => return Err(ParseError::UnknownNamed(name.to_string())),
    })
}

impl fmt::Display for Modifiers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        bitflags::parser::to_writer(self, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(ch: char) -> KeyChord {
        KeyChord::char(ch)
    }

    fn shift_k(ch: char) -> KeyChord {
        KeyChord {
            key: Key::Char(ch),
            modifiers: Modifiers::SHIFT,
        }
    }

    fn ctrl(ch: char) -> KeyChord {
        KeyChord {
            key: Key::Char(ch),
            modifiers: Modifiers::CTRL,
        }
    }

    #[test]
    fn empty_input() {
        assert_eq!(parse_keys("").unwrap(), vec![]);
    }

    #[test]
    fn bare_chars() {
        assert_eq!(parse_keys("gT").unwrap(), vec![k('g'), shift_k('T')]);
    }

    #[test]
    fn ctrl_w_v() {
        assert_eq!(parse_keys("<C-w>v").unwrap(), vec![ctrl('w'), k('v')]);
    }

    #[test]
    fn ctrl_shift_tab() {
        let chords = parse_keys("<C-S-Tab>").unwrap();
        assert_eq!(chords.len(), 1);
        assert_eq!(chords[0].key, Key::Named(NamedKey::Tab));
        assert!(chords[0].modifiers.contains(Modifiers::CTRL));
        assert!(chords[0].modifiers.contains(Modifiers::SHIFT));
    }

    #[test]
    fn shift_tab_alias() {
        let a = parse_keys("<S-Tab>").unwrap();
        let b = parse_keys("<BackTab>").unwrap();
        // <S-Tab> parses Tab + SHIFT; <BackTab> parses BackTab no
        // modifier. Both representations are valid emit paths from
        // the host depending on terminal/CEF.
        assert_eq!(a[0].key, Key::Named(NamedKey::Tab));
        assert!(a[0].modifiers.contains(Modifiers::SHIFT));
        assert_eq!(b[0].key, Key::Named(NamedKey::BackTab));
    }

    #[test]
    fn meta_alias_for_alt() {
        let m = parse_keys("<M-x>").unwrap();
        let a = parse_keys("<A-x>").unwrap();
        assert_eq!(m, a);
        assert!(m[0].modifiers.contains(Modifiers::ALT));
    }

    #[test]
    fn leader_is_abstract() {
        let chords = parse_keys("<leader>n").unwrap();
        assert_eq!(chords[0].key, Key::Named(NamedKey::Leader));
        assert_eq!(chords[1], k('n'));
    }

    #[test]
    fn space_special() {
        let chords = parse_keys("<Space>x").unwrap();
        assert_eq!(chords[0].key, Key::Named(NamedKey::Space));
        assert_eq!(chords[1], k('x'));
    }

    #[test]
    fn esc_aliases() {
        for s in ["<Esc>", "<escape>", "<ESC>"] {
            assert_eq!(
                parse_keys(s).unwrap(),
                vec![KeyChord::plain(Key::Named(NamedKey::Esc))],
                "alias {s}"
            );
        }
    }

    #[test]
    fn enter_cr_alias() {
        let cr = parse_keys("<CR>").unwrap();
        let enter = parse_keys("<Enter>").unwrap();
        let ret = parse_keys("<Return>").unwrap();
        assert_eq!(cr, enter);
        assert_eq!(cr, ret);
    }

    #[test]
    fn bs_aliases() {
        let bs = parse_keys("<BS>").unwrap();
        let bksp = parse_keys("<Backspace>").unwrap();
        assert_eq!(bs, bksp);
        assert_eq!(bs[0].key, Key::Named(NamedKey::BS));
    }

    #[test]
    fn lt_escape() {
        let chords = parse_keys("<lt>").unwrap();
        assert_eq!(chords, vec![k('<')]);
    }

    #[test]
    fn function_keys() {
        for n in 1..=12u8 {
            let s = format!("<F{n}>");
            let chords = parse_keys(&s).unwrap();
            assert_eq!(chords[0].key, Key::Named(NamedKey::F(n)));
        }
    }

    #[test]
    fn function_key_out_of_range_errors() {
        assert!(matches!(
            parse_keys("<F13>"),
            Err(ParseError::UnknownNamed(_))
        ));
        assert!(matches!(parse_keys("<F0>"), Err(ParseError::UnknownNamed(_))));
    }

    #[test]
    fn unclosed_bracket_errors() {
        assert_eq!(parse_keys("<C-w").unwrap_err(), ParseError::UnclosedBracket);
    }

    #[test]
    fn dangling_modifier_errors() {
        // `<C->` has the C- prefix but no following key.
        assert!(matches!(
            parse_keys("<C->"),
            Err(ParseError::DanglingModifier(_))
        ));
    }

    #[test]
    fn empty_bracket_errors() {
        assert!(matches!(parse_keys("<>"), Err(ParseError::EmptyBracket)));
    }

    #[test]
    fn unknown_named_errors() {
        match parse_keys("<NoSuchKey>") {
            Err(ParseError::UnknownNamed(name)) => {
                assert!(name.eq_ignore_ascii_case("nosuchkey"));
            }
            other => panic!("expected UnknownNamed, got {other:?}"),
        }
    }

    #[test]
    fn whitespace_ignored() {
        let chords = parse_keys("<C-w>  v   ").unwrap();
        assert_eq!(chords, vec![ctrl('w'), k('v')]);
    }

    #[test]
    fn modifier_order_independent() {
        let a = parse_keys("<C-S-Tab>").unwrap();
        let b = parse_keys("<S-C-Tab>").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn shift_letter_normalises_to_uppercase() {
        let a = parse_keys("<S-a>").unwrap();
        let b = parse_keys("<S-A>").unwrap();
        assert_eq!(a, b);
        assert_eq!(a[0].key, Key::Char('A'));
        assert!(a[0].modifiers.contains(Modifiers::SHIFT));
    }

    #[test]
    fn ctrl_letter_is_case_insensitive() {
        let a = parse_keys("<C-A>").unwrap();
        let b = parse_keys("<C-a>").unwrap();
        assert_eq!(a, b);
        assert_eq!(a[0].key, Key::Char('a'));
    }

    #[test]
    fn punctuation_passes_through() {
        let chords = parse_keys(":/?,;").unwrap();
        assert_eq!(chords, vec![k(':'), k('/'), k('?'), k(','), k(';')]);
    }

    #[test]
    fn embedded_literals_split_per_char() {
        // "Hello" → 5 chords; capital H carries SHIFT, ello are bare.
        let chords = parse_keys("Hello").unwrap();
        assert_eq!(chords.len(), 5);
        assert_eq!(chords[0], shift_k('H'));
        assert_eq!(chords[1], k('e'));
        assert_eq!(chords[4], k('o'));
    }

    #[test]
    fn register_prefix_quote_a() {
        // `"ay` is three chords: ", a, y. The Engine — not the
        // parser — recognises `"<char>` as a register selector.
        let chords = parse_keys("\"ay").unwrap();
        assert_eq!(chords, vec![k('"'), k('a'), k('y')]);
    }

    #[test]
    fn parse_key_single() {
        let kc = parse_key("<C-w>").unwrap();
        assert_eq!(kc, ctrl('w'));
    }

    #[test]
    fn parse_key_residual_errors() {
        assert!(matches!(
            parse_key("<C-w>v"),
            Err(ParseError::ExpectedOneChord(2))
        ));
        assert!(matches!(
            parse_key(""),
            Err(ParseError::ExpectedOneChord(0))
        ));
    }
}
