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
//!
//! # Layout
//!
//! - [`actions`] — [`PageAction`] / [`PageMode`] / [`Mode`]
//! - [`key`] — vim-notation parser → [`KeyChord`] / [`Modifiers`]
//! - [`keymap`] — mode-scoped trie + ambiguity resolution
//! - [`edit_mode`] — [`EditSession`] wrapping `hjkl_engine::Editor`
//! - [`host`] — [`BuffrHost`] adapter implementing
//!   `hjkl_engine::Host`

pub mod actions;
pub mod edit_mode;
pub mod engine;
pub mod host;
pub mod key;
pub mod keymap;

pub use actions::{Mode, PageAction, PageMode};
pub use edit_mode::EditSession;
pub use engine::{DEFAULT_TIMEOUT, EditModeStep, Engine, Step};
pub use host::{BuffrEditIntent, BuffrHost};
pub use key::{Key, KeyChord, Modifiers, NamedKey, ParseError, parse_key, parse_keys};
pub use keymap::{BindError, Keymap, Lookup};
