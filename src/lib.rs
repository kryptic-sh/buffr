//! Vim-style modal keybinding engine for buffr.
//!
//! Two-layer modal model:
//!
//! - **Page mode** ([`PageMode`]) — scroll, tab switch, omnibar, hint mode,
//!   command line. Owned by this crate.
//! - **Edit mode** — typing in `<textarea>` / `contenteditable` / form
//!   fields. Delegates to [`hjkl_engine::Editor`] against a mirrored
//!   [`hjkl_buffer::Buffer`] synced to the DOM via CEF.
//!
//! See `PLAN.md` "Edit-mode integration with `hjkl-*`" for the full
//! data flow.

use serde::{Deserialize, Serialize};

pub mod defaults;
pub mod edit_mode;
pub mod host;
pub mod keymap;
pub mod trie;

pub use defaults::vim_defaults;
pub use edit_mode::EditSession;
pub use host::BuffrHost;
pub use keymap::{ChordMods, KeyAtom, KeyChord, KeyParseError, SpecialKey, parse};
pub use trie::{Keymap, Lookup};

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

/// Page-mode FSM states. Distinct from [`Mode`] (the status-line summary)
/// — `PageMode` is what the keymap trie dispatches against.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PageMode {
    #[default]
    Normal,
    Visual,
    Command,
    Hint,
    /// A pending key sequence is being collected (e.g., `g…`, `<C-w>…`).
    Pending,
    /// Edit-mode is active; keystrokes route to the embedded
    /// `hjkl_engine::Editor`.
    Edit,
}

/// Page-level actions emitted by the modal dispatcher. The host (CEF
/// shell) translates each into a CEF command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageAction {
    ScrollUp(u32),
    ScrollDown(u32),
    ScrollLeft(u32),
    ScrollRight(u32),
    ScrollPageUp,
    ScrollPageDown,
    ScrollTop,
    ScrollBottom,

    TabNext,
    TabPrev,
    TabClose,
    TabNew,

    HistoryBack,
    HistoryForward,
    Reload,
    StopLoading,

    OpenOmnibar,
    OpenCommandLine,
    EnterHintMode,

    YankUrl,

    /// Defer to the embedded `hjkl_engine::Editor`. Keystroke unchanged;
    /// the modal dispatcher swallows nothing on this path.
    EnterEditMode,
}
