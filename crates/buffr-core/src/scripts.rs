//! Embedded JavaScript snippets executed via `run_js` against CEF
//! frames.
//!
//! Every script lives as a `.js` asset under `crates/buffr-core/assets/`
//! and is pulled in here via [`include_str!`]. Keeping the bodies in
//! their own files lets editors apply real syntax highlighting and
//! ESLint checks; the Rust side just wires the bytes through.

/// Focus the first VISIBLE editable text field on the page (`i` / `gi`).
pub const FOCUS_FIRST_INPUT: &str = include_str!("../assets/focus_first_input.js");

/// Exit insert/edit mode: synthesize Escape keydown+keyup then blur.
pub const EXIT_INSERT: &str = include_str!("../assets/exit_insert.js");
