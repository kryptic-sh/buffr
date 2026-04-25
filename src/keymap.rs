//! Vim-notation key parser.
//!
//! Parses strings like `<C-w>v`, `gT`, `<leader>fa`, `<C-S-Tab>` into
//! a list of [`KeyChord`]s the page-mode dispatcher can match against.
//!
//! Notation supported:
//!
//! - `<C-...>` — Ctrl
//! - `<S-...>` — Shift (also implied by uppercase ASCII)
//! - `<M-...>` / `<A-...>` — Alt
//! - `<D-...>` — Super (Cmd on macOS)
//! - `<leader>` — placeholder for the configured leader key (default
//!   `\`); resolution happens at trie-build time, not here.
//! - `<Space>`, `<Esc>`, `<CR>` / `<Enter>`, `<BS>` / `<Backspace>`,
//!   `<Tab>`, `<S-Tab>` / `<BackTab>`, `<Up>`, `<Down>`, `<Left>`,
//!   `<Right>`, `<Home>`, `<End>`, `<PageUp>`, `<PageDown>`,
//!   `<Insert>`, `<Delete>`, `<F1>`–`<F12>`
//! - Bare chars, including punctuation: `g`, `T`, `,`, `;`, `:`, `/`,
//!   `?`, `<` (escaped as `<lt>`).
//!
//! Whitespace and CRs in the input are ignored. Empty input yields an
//! empty `Vec`. Malformed `<...>` blocks fall through to literal `<`
//! plus the inner text.

use std::fmt;

/// One position in a multi-key chord.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyChord {
    pub key: KeyAtom,
    pub mods: ChordMods,
}

/// What was pressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyAtom {
    Char(char),
    Special(SpecialKey),
    Leader,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChordMods {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub super_: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SpecialKey {
    Esc,
    Enter,
    Backspace,
    Tab,
    BackTab,
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
    Space,
    F(u8),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyParseError {
    UnknownNamed(String),
    UnclosedBracket,
}

impl fmt::Display for KeyParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyParseError::UnknownNamed(s) => write!(f, "unknown key name <{s}>"),
            KeyParseError::UnclosedBracket => write!(f, "unclosed `<` in key notation"),
        }
    }
}

impl std::error::Error for KeyParseError {}

/// Parse `input` into a sequence of [`KeyChord`]s.
pub fn parse(input: &str) -> Result<Vec<KeyChord>, KeyParseError> {
    let mut out = Vec::new();
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        // Drop ASCII whitespace between chords; consumers may format
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
                return Err(KeyParseError::UnclosedBracket);
            }
            out.push(parse_named(&name)?);
        } else {
            out.push(parse_char(c));
        }
    }
    Ok(out)
}

fn parse_char(c: char) -> KeyChord {
    let mut mods = ChordMods::default();
    if c.is_ascii_uppercase() {
        mods.shift = true;
    }
    KeyChord {
        key: KeyAtom::Char(c),
        mods,
    }
}

fn parse_named(raw: &str) -> Result<KeyChord, KeyParseError> {
    // Special: `<lt>` is the literal `<`.
    if raw.eq_ignore_ascii_case("lt") {
        return Ok(KeyChord {
            key: KeyAtom::Char('<'),
            mods: ChordMods::default(),
        });
    }

    let (mods, tail) = parse_modifiers(raw);

    // Resolve the tail.
    let key = if tail.eq_ignore_ascii_case("leader") {
        KeyAtom::Leader
    } else if tail.len() == 1 {
        let ch = tail.chars().next().unwrap();
        // Ctrl+letter is case-insensitive; preserve user's case
        // otherwise so `<S-a>` and `<S-A>` parse the same.
        let ch = if mods.ctrl {
            ch.to_ascii_lowercase()
        } else if mods.shift && ch.is_ascii_alphabetic() {
            ch.to_ascii_uppercase()
        } else {
            ch
        };
        KeyAtom::Char(ch)
    } else {
        KeyAtom::Special(parse_special(tail)?)
    };

    Ok(KeyChord { key, mods })
}

/// Strip leading `C-` / `S-` / `M-` / `A-` / `D-` prefixes (ASCII
/// case-insensitive) and return the remaining tail.
fn parse_modifiers(raw: &str) -> (ChordMods, &str) {
    let mut mods = ChordMods::default();
    let mut tail = raw;
    loop {
        let lower_prefix = tail.get(..2).map(str::to_ascii_lowercase);
        match lower_prefix.as_deref() {
            Some("c-") => {
                mods.ctrl = true;
                tail = &tail[2..];
            }
            Some("s-") => {
                mods.shift = true;
                tail = &tail[2..];
            }
            Some("m-") | Some("a-") => {
                mods.alt = true;
                tail = &tail[2..];
            }
            Some("d-") => {
                mods.super_ = true;
                tail = &tail[2..];
            }
            _ => return (mods, tail),
        }
    }
}

fn parse_special(name: &str) -> Result<SpecialKey, KeyParseError> {
    let n = name.to_ascii_lowercase();
    Ok(match n.as_str() {
        "esc" | "escape" => SpecialKey::Esc,
        "cr" | "enter" | "return" => SpecialKey::Enter,
        "bs" | "backspace" => SpecialKey::Backspace,
        "tab" => SpecialKey::Tab,
        "backtab" => SpecialKey::BackTab,
        "space" => SpecialKey::Space,
        "up" => SpecialKey::Up,
        "down" => SpecialKey::Down,
        "left" => SpecialKey::Left,
        "right" => SpecialKey::Right,
        "home" => SpecialKey::Home,
        "end" => SpecialKey::End,
        "pageup" | "pgup" => SpecialKey::PageUp,
        "pagedown" | "pgdn" => SpecialKey::PageDown,
        "insert" | "ins" => SpecialKey::Insert,
        "delete" | "del" => SpecialKey::Delete,
        s if s.starts_with('f') => {
            let num: u8 = s[1..]
                .parse()
                .map_err(|_| KeyParseError::UnknownNamed(name.to_string()))?;
            if !(1..=12).contains(&num) {
                return Err(KeyParseError::UnknownNamed(name.to_string()));
            }
            SpecialKey::F(num)
        }
        _ => return Err(KeyParseError::UnknownNamed(name.to_string())),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(ch: char) -> KeyChord {
        KeyChord {
            key: KeyAtom::Char(ch),
            mods: ChordMods::default(),
        }
    }

    fn shift_k(ch: char) -> KeyChord {
        KeyChord {
            key: KeyAtom::Char(ch),
            mods: ChordMods {
                shift: true,
                ..ChordMods::default()
            },
        }
    }

    fn ctrl(ch: char) -> KeyChord {
        KeyChord {
            key: KeyAtom::Char(ch),
            mods: ChordMods {
                ctrl: true,
                ..ChordMods::default()
            },
        }
    }

    #[test]
    fn empty_input() {
        assert_eq!(parse("").unwrap(), vec![]);
    }

    #[test]
    fn bare_chars() {
        assert_eq!(parse("gT").unwrap(), vec![k('g'), shift_k('T')]);
    }

    #[test]
    fn ctrl_w_v() {
        let chords = parse("<C-w>v").unwrap();
        assert_eq!(chords, vec![ctrl('w'), k('v')]);
    }

    #[test]
    fn ctrl_shift_tab() {
        let chords = parse("<C-S-Tab>").unwrap();
        assert_eq!(chords.len(), 1);
        assert_eq!(chords[0].key, KeyAtom::Special(SpecialKey::Tab));
        assert!(chords[0].mods.ctrl);
        assert!(chords[0].mods.shift);
    }

    #[test]
    fn leader_resolves_to_atom() {
        let chords = parse("<leader>fa").unwrap();
        assert_eq!(chords[0].key, KeyAtom::Leader);
        assert_eq!(chords[1], k('f'));
        assert_eq!(chords[2], k('a'));
    }

    #[test]
    fn space_special() {
        let chords = parse("<Space>x").unwrap();
        assert_eq!(chords[0].key, KeyAtom::Special(SpecialKey::Space));
        assert_eq!(chords[1], k('x'));
    }

    #[test]
    fn esc_aliases() {
        for s in ["<Esc>", "<escape>", "<ESC>"] {
            assert_eq!(
                parse(s).unwrap(),
                vec![KeyChord {
                    key: KeyAtom::Special(SpecialKey::Esc),
                    mods: ChordMods::default(),
                }],
                "alias {s}"
            );
        }
    }

    #[test]
    fn enter_cr_alias() {
        let cr = parse("<CR>").unwrap();
        let enter = parse("<Enter>").unwrap();
        assert_eq!(cr, enter);
    }

    #[test]
    fn lt_escape() {
        let chords = parse("<lt>").unwrap();
        assert_eq!(chords, vec![k('<')]);
    }

    #[test]
    fn function_keys() {
        for n in 1..=12u8 {
            let s = format!("<F{n}>");
            let chords = parse(&s).unwrap();
            assert_eq!(chords[0].key, KeyAtom::Special(SpecialKey::F(n)));
        }
    }

    #[test]
    fn function_key_out_of_range_errors() {
        assert!(matches!(
            parse("<F13>"),
            Err(KeyParseError::UnknownNamed(_))
        ));
        assert!(matches!(parse("<F0>"), Err(KeyParseError::UnknownNamed(_))));
    }

    #[test]
    fn unclosed_bracket_errors() {
        assert_eq!(parse("<C-w").unwrap_err(), KeyParseError::UnclosedBracket);
    }

    #[test]
    fn unknown_named_errors() {
        match parse("<Frobnicate>") {
            Err(KeyParseError::UnknownNamed(name)) => {
                assert!(name.eq_ignore_ascii_case("frobnicate"))
            }
            other => panic!("expected UnknownNamed, got {other:?}"),
        }
    }

    #[test]
    fn whitespace_ignored() {
        let chords = parse("<C-w>  v   ").unwrap();
        assert_eq!(chords, vec![ctrl('w'), k('v')]);
    }

    #[test]
    fn modifier_order_independent() {
        let a = parse("<C-S-Tab>").unwrap();
        let b = parse("<S-C-Tab>").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn shift_letter_normalises_to_uppercase() {
        // `<S-a>` parses to the same chord as `<S-A>`.
        let a = parse("<S-a>").unwrap();
        let b = parse("<S-A>").unwrap();
        assert_eq!(a, b);
        assert_eq!(a[0].key, KeyAtom::Char('A'));
        assert!(a[0].mods.shift);
    }

    #[test]
    fn ctrl_letter_is_case_insensitive() {
        // Ctrl chord is `Ctrl+lowercase` regardless of how the user wrote it.
        let a = parse("<C-A>").unwrap();
        let b = parse("<C-a>").unwrap();
        assert_eq!(a, b);
        assert_eq!(a[0].key, KeyAtom::Char('a'));
    }

    #[test]
    fn punctuation_passes_through() {
        let chords = parse(":/?,;").unwrap();
        assert_eq!(chords, vec![k(':'), k('/'), k('?'), k(','), k(';')]);
    }
}
