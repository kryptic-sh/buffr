//! Page-mode action enum + mode states.
//!
//! [`PageAction`] is what the keymap dispatcher emits. The host (CEF
//! shell) translates each into a CEF command. Variants here include
//! both nullary actions (`Reload`, `TabClose`) and count-bearing
//! scrolls (`ScrollDown(u32)`).
//!
//! Mode-transition actions (`OpenOmnibar`, `EnterHintMode`,
//! `EnterInsertMode`, `EnterMode`) are emitted to the host *and* drive
//! [`crate::engine::Engine::set_mode`] at the same point — see the
//! design note at the top of `engine.rs`.

use serde::{Deserialize, Serialize};

/// Coarse mode displayed in the status line. `Insert` is a single state
/// here even though `hjkl_engine` may be in Normal/Insert/Visual
/// internally — the page-mode FSM doesn't care which sub-mode the
/// embedded editor is in, only that page-level keystrokes route to
/// `BuffrHost` instead of the page action dispatcher.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Mode {
    #[default]
    Normal,
    Visual,
    Command,
    Hint,
    Insert,
}

/// Page-mode FSM states. Distinct from [`Mode`] (the status-line
/// summary) — `PageMode` is what the keymap trie dispatches against.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PageMode {
    #[default]
    Normal,
    Visual,
    Command,
    Hint,
    /// A pending key sequence is being collected (e.g., `g…`,
    /// `<C-w>…`). Internal — surfaces from `Engine::mode()` only while
    /// a multi-chord prefix is mid-flight.
    Pending,
    /// Insert-mode is active; keystrokes route through
    /// [`crate::engine::Engine::feed_edit_mode_key`] which (post-Phase
    /// 2) hands off to `hjkl_editor::Editor`.
    Insert,
}

/// Page-level actions emitted by the modal dispatcher.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PageAction {
    // -- scroll --------------------------------------------------------
    ScrollUp(u32),
    ScrollDown(u32),
    ScrollLeft(u32),
    ScrollRight(u32),
    ScrollPageUp,
    ScrollPageDown,
    /// `<C-f>` — full-window forward scroll.
    ScrollFullPageDown,
    /// `<C-b>` — full-window back scroll.
    ScrollFullPageUp,
    ScrollHalfPageDown,
    ScrollHalfPageUp,
    ScrollTop,
    ScrollBottom,

    // -- tabs ---------------------------------------------------------
    TabNext,
    TabPrev,
    TabClose,
    TabNew,
    /// Open a fresh tab adjacent to the active tab. The apps layer also
    /// auto-opens the omnibar after creation so the user can type a URL.
    /// Inserts the new tab immediately to the right of the active tab.
    TabNewRight,
    /// Open a fresh tab adjacent to the active tab. The apps layer also
    /// auto-opens the omnibar after creation so the user can type a URL.
    /// Inserts the new tab immediately to the left of the active tab.
    TabNewLeft,
    /// Duplicate the active tab — clones the URL into a fresh tab.
    /// Default keybind `<C-w>n` (see `docs/keymap.md`).
    DuplicateTab,
    /// Pin / unpin the active tab. Pinned tabs sort first; pin does
    /// **not** prevent close. Default keybind `<C-w>p`.
    PinTab,
    /// Reorder the tab list. Currently unbound; reserved for the
    /// eventual mouse-drag handler.
    TabReorder {
        from: u32,
        to: u32,
    },

    // -- history ------------------------------------------------------
    HistoryBack,
    HistoryForward,
    Reload,
    /// Hard reload bypassing cache (`<C-r>`).
    ReloadHard,
    StopLoading,

    // -- mode transitions --------------------------------------------
    OpenOmnibar,
    OpenCommandLine,
    EnterHintMode,
    /// Background-tab variant of hint mode (`F` in vimium).
    EnterHintModeBackground,
    /// Generic mode-transition variant. Equivalent to the more
    /// specific `OpenOmnibar`/`OpenCommandLine`/`EnterHintMode`/
    /// `EnterInsertMode` actions but parameterised — useful for user
    /// config that wants `<F2>` → command mode.
    EnterMode(PageMode),

    // -- find ---------------------------------------------------------
    /// `/` (forward) or `?` (backward).
    Find {
        forward: bool,
    },
    FindNext,
    FindPrev,

    // -- yank ---------------------------------------------------------
    /// Yank the current page URL. Phase 2 emits the action; clipboard
    /// plumbing lands with the host wiring in `buffr-core`.
    YankUrl,

    // -- zoom ---------------------------------------------------------
    ZoomIn,
    ZoomOut,
    ZoomReset,

    // -- devtools / misc ---------------------------------------------
    OpenDevTools,

    // -- downloads ----------------------------------------------------
    /// Delete every `Completed` download row. Does not have a default
    /// keybinding — there's no obvious vim-flavored chord — so it's
    /// reachable only via user config (`[keymap.normal] "..." =
    /// "clear_completed_downloads"`) or the eventual `:downloads`
    /// command line in Phase 3 chrome work.
    ClearCompletedDownloads,

    /// Defer to the embedded `hjkl_engine::Editor`. Keystroke
    /// unchanged; the modal dispatcher swallows nothing on this path.
    EnterInsertMode,

    /// Focus the first text input on the page and enter insert mode.
    /// Vieb's `gi` / `insertAtFirstInput`.
    FocusFirstInput,

    /// Exit insert mode unconditionally — blurs the focused DOM element
    /// and returns the engine to PageMode::Normal.
    ExitInsertMode,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_mode_serde_round_trip() {
        let s = toml::to_string(&Wrap {
            m: PageMode::Visual,
        })
        .unwrap();
        let back: Wrap = toml::from_str(&s).unwrap();
        assert_eq!(back.m, PageMode::Visual);
    }

    #[test]
    fn page_action_serde_round_trip_unit() {
        let s = toml::to_string(&Wrap2 {
            a: PageAction::Reload,
        })
        .unwrap();
        let back: Wrap2 = toml::from_str(&s).unwrap();
        assert_eq!(back.a, PageAction::Reload);
    }

    #[test]
    fn page_action_serde_round_trip_count() {
        let s = toml::to_string(&Wrap2 {
            a: PageAction::ScrollDown(5),
        })
        .unwrap();
        let back: Wrap2 = toml::from_str(&s).unwrap();
        assert_eq!(back.a, PageAction::ScrollDown(5));
    }

    #[derive(Serialize, Deserialize)]
    struct Wrap {
        m: PageMode,
    }

    #[derive(Serialize, Deserialize)]
    struct Wrap2 {
        a: PageAction,
    }
}
