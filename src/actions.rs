//! Page-mode action enum + mode states.
//!
//! [`PageAction`] is what the keymap dispatcher emits. The host (CEF
//! shell) translates each into a CEF command. Variants here include
//! both nullary actions (`Reload`, `TabClose`) and count-bearing
//! scrolls (`ScrollDown(u32)`).
//!
//! Mode-transition actions (`OpenOmnibar`, `EnterHintMode`,
//! `EnterEditMode`, `EnterMode`) are emitted to the host *and* drive
//! [`crate::engine::Engine::set_mode`] at the same point — see the
//! design note at the top of `engine.rs`.

use serde::{Deserialize, Serialize};

/// Coarse mode displayed in the status line. `Edit` is a single state
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
    Edit,
}

/// Page-mode FSM states. Distinct from [`Mode`] (the status-line
/// summary) — `PageMode` is what the keymap trie dispatches against.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
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
    /// Edit-mode is active; keystrokes route through
    /// [`crate::engine::Engine::feed_edit_mode_key`] which (post-Phase
    /// 2) hands off to `hjkl_editor::Editor`.
    Edit,
}

/// Page-level actions emitted by the modal dispatcher.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// `EnterEditMode` actions but parameterised — useful for user
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

    /// Defer to the embedded `hjkl_engine::Editor`. Keystroke
    /// unchanged; the modal dispatcher swallows nothing on this path.
    EnterEditMode,
}
