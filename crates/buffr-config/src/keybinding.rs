//! TOML string → [`PageAction`] parser.
//!
//! Format:
//!
//! - Bare snake_case identifier → unit variant. Example: `"scroll_down"`.
//! - `name(N)` for count-bearing scrolls. Example: `"scroll_down(3)"`.
//! - `find(forward = true|false)` for the boolean-tagged find action.
//! - `enter_mode("normal" | "visual" | "command" | "hint")` for mode
//!   transitions.
//!
//! Anything else returns a clear error that mentions the offending token.

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

use buffr_modal::{PageAction, PageMode};

/// Wrapper around [`PageAction`] that deserializes from a TOML string.
///
/// Round-trips through `serde::Serialize` by writing the canonical
/// notation back out (so `--print-config` produces parseable output).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyBinding {
    pub action: PageAction,
}

impl<'de> Deserialize<'de> for KeyBinding {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        let action = parse_action(&s).map_err(serde::de::Error::custom)?;
        Ok(KeyBinding { action })
    }
}

impl Serialize for KeyBinding {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&action_to_string(&self.action))
    }
}

#[derive(Debug, Error)]
pub enum KeyBindingError {
    #[error("unknown action: {0:?}")]
    UnknownAction(String),
    #[error("malformed action notation: {0:?}")]
    Malformed(String),
    #[error("invalid argument for {action}: {msg}")]
    BadArg { action: String, msg: String },
}

/// Parse a binding string into a [`PageAction`].
pub fn parse_action(s: &str) -> Result<PageAction, KeyBindingError> {
    let s = s.trim();
    if let Some(open) = s.find('(') {
        if !s.ends_with(')') {
            return Err(KeyBindingError::Malformed(s.into()));
        }
        let name = s[..open].trim();
        let inner = &s[open + 1..s.len() - 1];
        return parse_with_args(name, inner.trim());
    }
    parse_unit(s)
}

fn parse_unit(name: &str) -> Result<PageAction, KeyBindingError> {
    use PageAction::*;
    Ok(match name {
        "scroll_up" => ScrollUp(1),
        "scroll_down" => ScrollDown(1),
        "scroll_left" => ScrollLeft(1),
        "scroll_right" => ScrollRight(1),
        "scroll_page_up" => ScrollPageUp,
        "scroll_page_down" => ScrollPageDown,
        "scroll_full_page_down" => ScrollFullPageDown,
        "scroll_full_page_up" => ScrollFullPageUp,
        "scroll_half_page_down" => ScrollHalfPageDown,
        "scroll_half_page_up" => ScrollHalfPageUp,
        "scroll_top" => ScrollTop,
        "scroll_bottom" => ScrollBottom,
        "tab_next" => TabNext,
        "tab_prev" => TabPrev,
        "tab_close" => TabClose,
        "tab_new" => TabNew,
        "tab_new_right" => TabNewRight,
        "tab_new_left" => TabNewLeft,
        "duplicate_tab" => DuplicateTab,
        "pin_tab" => PinTab,
        "reopen_closed_tab" => ReopenClosedTab,
        "history_back" => HistoryBack,
        "history_forward" => HistoryForward,
        "reload" => Reload,
        "reload_hard" => ReloadHard,
        "stop_loading" => StopLoading,
        "open_omnibar" => OpenOmnibar,
        "open_command_line" => OpenCommandLine,
        "enter_hint_mode" => EnterHintMode,
        "enter_hint_mode_background" => EnterHintModeBackground,
        "find_next" => FindNext,
        "find_prev" => FindPrev,
        "yank_url" => YankUrl,
        "zoom_in" => ZoomIn,
        "zoom_out" => ZoomOut,
        "zoom_reset" => ZoomReset,
        "open_dev_tools" => OpenDevTools,
        "clear_completed_downloads" => ClearCompletedDownloads,
        "enter_insert_mode" => EnterInsertMode,
        "focus_first_input" => FocusFirstInput,
        "exit_insert_mode" => ExitInsertMode,
        other => return Err(KeyBindingError::UnknownAction(other.into())),
    })
}

fn parse_with_args(name: &str, args: &str) -> Result<PageAction, KeyBindingError> {
    use PageAction::*;
    match name {
        "scroll_up" | "scroll_down" | "scroll_left" | "scroll_right" => {
            let n: u32 = args.parse().map_err(|_| KeyBindingError::BadArg {
                action: name.into(),
                msg: format!("expected non-negative integer, got {args:?}"),
            })?;
            Ok(match name {
                "scroll_up" => ScrollUp(n),
                "scroll_down" => ScrollDown(n),
                "scroll_left" => ScrollLeft(n),
                "scroll_right" => ScrollRight(n),
                _ => unreachable!(),
            })
        }
        "find" => {
            let forward =
                parse_kv_bool(args, "forward").map_err(|msg| KeyBindingError::BadArg {
                    action: "find".into(),
                    msg,
                })?;
            Ok(Find { forward })
        }
        "enter_mode" => {
            let arg = strip_string_lit(args).ok_or_else(|| KeyBindingError::BadArg {
                action: "enter_mode".into(),
                msg: format!("expected quoted mode name, got {args:?}"),
            })?;
            let mode = match arg {
                "normal" => PageMode::Normal,
                "visual" => PageMode::Visual,
                "command" => PageMode::Command,
                "hint" => PageMode::Hint,
                other => {
                    return Err(KeyBindingError::BadArg {
                        action: "enter_mode".into(),
                        msg: format!("unknown mode {other:?}"),
                    });
                }
            };
            Ok(EnterMode(mode))
        }
        other => Err(KeyBindingError::UnknownAction(other.into())),
    }
}

fn parse_kv_bool(s: &str, key: &str) -> Result<bool, String> {
    let parts: Vec<&str> = s.splitn(2, '=').collect();
    if parts.len() != 2 {
        return Err(format!("expected `{key} = true|false`, got {s:?}"));
    }
    let k = parts[0].trim();
    let v = parts[1].trim();
    if k != key {
        return Err(format!("expected key {key:?}, got {k:?}"));
    }
    match v {
        "true" => Ok(true),
        "false" => Ok(false),
        other => Err(format!("expected bool literal, got {other:?}")),
    }
}

fn strip_string_lit(s: &str) -> Option<&str> {
    let s = s.trim();
    if (s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')) {
        let len = s.len();
        if len >= 2 {
            return Some(&s[1..len - 1]);
        }
    }
    None
}

fn action_to_string(action: &PageAction) -> String {
    use PageAction::*;
    match action {
        ScrollUp(n) => format!("scroll_up({n})"),
        ScrollDown(n) => format!("scroll_down({n})"),
        ScrollLeft(n) => format!("scroll_left({n})"),
        ScrollRight(n) => format!("scroll_right({n})"),
        ScrollPageUp => "scroll_page_up".into(),
        ScrollPageDown => "scroll_page_down".into(),
        ScrollFullPageDown => "scroll_full_page_down".into(),
        ScrollFullPageUp => "scroll_full_page_up".into(),
        ScrollHalfPageDown => "scroll_half_page_down".into(),
        ScrollHalfPageUp => "scroll_half_page_up".into(),
        ScrollTop => "scroll_top".into(),
        ScrollBottom => "scroll_bottom".into(),
        TabNext => "tab_next".into(),
        TabPrev => "tab_prev".into(),
        TabClose => "tab_close".into(),
        TabNew => "tab_new".into(),
        TabNewRight => "tab_new_right".into(),
        TabNewLeft => "tab_new_left".into(),
        DuplicateTab => "duplicate_tab".into(),
        PinTab => "pin_tab".into(),
        ReopenClosedTab => "reopen_closed_tab".into(),
        TabReorder { from, to } => format!("tab_reorder(from = {from}, to = {to})"),
        HistoryBack => "history_back".into(),
        HistoryForward => "history_forward".into(),
        Reload => "reload".into(),
        ReloadHard => "reload_hard".into(),
        StopLoading => "stop_loading".into(),
        OpenOmnibar => "open_omnibar".into(),
        OpenCommandLine => "open_command_line".into(),
        EnterHintMode => "enter_hint_mode".into(),
        EnterHintModeBackground => "enter_hint_mode_background".into(),
        EnterMode(m) => format!("enter_mode({:?})", mode_name(*m)),
        Find { forward } => format!("find(forward = {forward})"),
        FindNext => "find_next".into(),
        FindPrev => "find_prev".into(),
        YankUrl => "yank_url".into(),
        ZoomIn => "zoom_in".into(),
        ZoomOut => "zoom_out".into(),
        ZoomReset => "zoom_reset".into(),
        OpenDevTools => "open_dev_tools".into(),
        ClearCompletedDownloads => "clear_completed_downloads".into(),
        EnterInsertMode => "enter_insert_mode".into(),
        FocusFirstInput => "focus_first_input".into(),
        ExitInsertMode => "exit_insert_mode".into(),
    }
}

fn mode_name(m: PageMode) -> &'static str {
    match m {
        PageMode::Normal => "normal",
        PageMode::Visual => "visual",
        PageMode::Command => "command",
        PageMode::Hint => "hint",
        PageMode::Pending => "pending",
        PageMode::Insert => "insert",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_variant() {
        assert_eq!(
            parse_action("scroll_down").unwrap(),
            PageAction::ScrollDown(1)
        );
        assert_eq!(parse_action("reload").unwrap(), PageAction::Reload);
    }

    #[test]
    fn count_variant() {
        assert_eq!(
            parse_action("scroll_down(5)").unwrap(),
            PageAction::ScrollDown(5)
        );
        assert_eq!(
            parse_action("scroll_up(0)").unwrap(),
            PageAction::ScrollUp(0)
        );
    }

    #[test]
    fn find_with_forward() {
        assert_eq!(
            parse_action("find(forward = true)").unwrap(),
            PageAction::Find { forward: true }
        );
        assert_eq!(
            parse_action("find(forward = false)").unwrap(),
            PageAction::Find { forward: false }
        );
    }

    #[test]
    fn enter_mode_quoted() {
        assert_eq!(
            parse_action("enter_mode(\"normal\")").unwrap(),
            PageAction::EnterMode(PageMode::Normal)
        );
    }

    #[test]
    fn unknown_errors() {
        assert!(matches!(
            parse_action("fly"),
            Err(KeyBindingError::UnknownAction(_))
        ));
    }

    #[test]
    fn round_trip_serialise() {
        for action in [
            PageAction::ScrollDown(3),
            PageAction::Reload,
            PageAction::Find { forward: false },
            PageAction::EnterMode(PageMode::Visual),
        ] {
            let s = action_to_string(&action);
            let back = parse_action(&s).unwrap();
            assert_eq!(back, action, "round trip failed for {s}");
        }
    }
}
