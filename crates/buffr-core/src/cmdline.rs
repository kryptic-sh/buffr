//! Ex-style command-line parser for the `:`-prefix command bar.
//!
//! Phase 3 chrome: the user presses `:` to open the command line, types
//! a command, and presses Enter. We split the buffer on whitespace,
//! match the first word against a fixed keyword set, and produce a
//! [`Command`] for `apps/buffr` to dispatch.
//!
//! The parser is deliberately dumb — no escaping, no quoting, no
//! variable expansion. Phase 6 may add a richer grammar; for now the
//! supported commands are listed in [`Command`].
//!
//! Unknown input lands in [`Command::Unknown`] with the **trimmed**
//! input verbatim so the error display can echo it back to the user.

/// Parsed command from the command-line buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `:q`, `:quit` — exit the application.
    Quit,
    /// `:reload` — hard-reload the page.
    Reload,
    /// `:back` — history back.
    Back,
    /// `:forward` — history forward.
    Forward,
    /// `:open <url>` — load `<url>` in the current tab.
    Open(String),
    /// `:tabnew` — open a fresh tab. Logs only in Phase 3 (multi-tab
    /// is Phase 5).
    TabNew,
    /// `:set <key> <value>` — adjust a runtime setting. Phase 3
    /// supports `set zoom <level>`; the apps layer maps to the right
    /// `BrowserHost` call.
    Set { key: String, value: String },
    /// `:find <query>` — kick off a find-in-page search.
    Find(String),
    /// `:bookmark <tags...>` — bookmark the current page with optional
    /// tags. Phase 3 logs only; UI for editing the title comes later.
    Bookmark { tags: Vec<String> },
    /// `:devtools` — open DevTools.
    DevTools,
    /// Anything we can't classify. Carries the trimmed original input
    /// for error display ("E: not an editor command: foo").
    Unknown(String),
}

/// Parse a command-line buffer (without the leading `:`) into a
/// [`Command`]. Whitespace-only input → [`Command::Unknown`] with an
/// empty payload. Input MUST already have its leading `:` stripped by
/// the caller — the parser doesn't know about prefixes.
pub fn parse(input: &str) -> Command {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Command::Unknown(String::new());
    }
    let mut parts = trimmed.split_whitespace();
    let head = match parts.next() {
        Some(h) => h,
        None => return Command::Unknown(trimmed.to_string()),
    };
    let rest: Vec<&str> = parts.collect();
    match head {
        "q" | "quit" => Command::Quit,
        "reload" => Command::Reload,
        "back" => Command::Back,
        "forward" => Command::Forward,
        "tabnew" => Command::TabNew,
        "devtools" => Command::DevTools,
        "open" => {
            if rest.is_empty() {
                Command::Unknown(trimmed.to_string())
            } else {
                Command::Open(rest.join(" "))
            }
        }
        "find" => {
            if rest.is_empty() {
                Command::Unknown(trimmed.to_string())
            } else {
                Command::Find(rest.join(" "))
            }
        }
        "set" => {
            if rest.len() != 2 {
                Command::Unknown(trimmed.to_string())
            } else {
                Command::Set {
                    key: rest[0].to_string(),
                    value: rest[1].to_string(),
                }
            }
        }
        "bookmark" => Command::Bookmark {
            tags: rest.iter().map(|s| s.to_string()).collect(),
        },
        _ => Command::Unknown(trimmed.to_string()),
    }
}

/// Static list of `:` command names for omnibar-style completion.
/// Sorted alphabetically so the dropdown looks deterministic.
pub const COMMAND_NAMES: &[&str] = &[
    "back", "bookmark", "devtools", "find", "forward", "open", "q", "quit", "reload", "set",
    "tabnew",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quit_aliases() {
        assert_eq!(parse("q"), Command::Quit);
        assert_eq!(parse("quit"), Command::Quit);
    }

    #[test]
    fn reload() {
        assert_eq!(parse("reload"), Command::Reload);
    }

    #[test]
    fn back_forward() {
        assert_eq!(parse("back"), Command::Back);
        assert_eq!(parse("forward"), Command::Forward);
    }

    #[test]
    fn tabnew() {
        assert_eq!(parse("tabnew"), Command::TabNew);
    }

    #[test]
    fn devtools() {
        assert_eq!(parse("devtools"), Command::DevTools);
    }

    #[test]
    fn open_with_url() {
        assert_eq!(
            parse("open https://example.com"),
            Command::Open("https://example.com".into())
        );
    }

    #[test]
    fn open_without_arg_is_unknown() {
        assert_eq!(parse("open"), Command::Unknown("open".into()));
    }

    #[test]
    fn find_with_query() {
        assert_eq!(parse("find rust"), Command::Find("rust".into()));
    }

    #[test]
    fn find_multi_word_query() {
        assert_eq!(
            parse("find hello world"),
            Command::Find("hello world".into())
        );
    }

    #[test]
    fn find_without_arg_is_unknown() {
        assert_eq!(parse("find"), Command::Unknown("find".into()));
    }

    #[test]
    fn set_two_args() {
        assert_eq!(
            parse("set zoom 1.5"),
            Command::Set {
                key: "zoom".into(),
                value: "1.5".into(),
            }
        );
    }

    #[test]
    fn set_three_args_is_unknown() {
        assert_eq!(
            parse("set zoom 1 extra"),
            Command::Unknown("set zoom 1 extra".into())
        );
    }

    #[test]
    fn set_one_arg_is_unknown() {
        assert_eq!(parse("set zoom"), Command::Unknown("set zoom".into()));
    }

    #[test]
    fn bookmark_no_tags() {
        assert_eq!(parse("bookmark"), Command::Bookmark { tags: vec![] });
    }

    #[test]
    fn bookmark_with_tags() {
        assert_eq!(
            parse("bookmark a b c"),
            Command::Bookmark {
                tags: vec!["a".into(), "b".into(), "c".into()],
            }
        );
    }

    #[test]
    fn unknown_command() {
        assert_eq!(parse("foo"), Command::Unknown("foo".into()));
    }

    #[test]
    fn empty_input() {
        assert_eq!(parse(""), Command::Unknown(String::new()));
        assert_eq!(parse("   "), Command::Unknown(String::new()));
    }

    #[test]
    fn whitespace_collapsed_in_args() {
        // Multiple spaces between tokens are allowed.
        assert_eq!(
            parse("open   https://x.example"),
            Command::Open("https://x.example".into())
        );
    }
}
