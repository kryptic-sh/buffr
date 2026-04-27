//! Edit-mode session — wires `hjkl_engine::Editor` to a [`BuffrHost`]
//! against an in-memory text buffer.
//!
//! In production this is fed by CEF V8 bindings: text-field focus
//! events seed [`EditSession`] from the field's value, keystrokes get
//! forwarded as [`KeyEvent`]s, and the host's tick loop drains
//! [`EditSession::take_content_change`] to push DOM updates back via
//! the JS bridge. The in-memory shape here is identical to that path
//! sans CEF, which keeps the integration testable without a browser.

use crate::host::BuffrHost;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hjkl_engine::{
    Editor, KeybindingMode, Modifiers, PlannedInput, SpecialKey, VimMode, types::Options,
};
use std::sync::Arc;

/// Convert a crossterm [`KeyEvent`] into the engine's [`PlannedInput`].
///
/// Uses the crossterm-free `feed_input` path so the engine and buffr
/// can carry different crossterm major versions without a type mismatch.
fn key_event_to_planned(key: KeyEvent) -> PlannedInput {
    let mods = Modifiers {
        ctrl: key.modifiers.contains(KeyModifiers::CONTROL),
        shift: key.modifiers.contains(KeyModifiers::SHIFT),
        alt: key.modifiers.contains(KeyModifiers::ALT),
        super_: key.modifiers.contains(KeyModifiers::SUPER),
    };
    match key.code {
        KeyCode::Char(c) => PlannedInput::Char(c, mods),
        KeyCode::Esc => PlannedInput::Key(SpecialKey::Esc, mods),
        KeyCode::Enter => PlannedInput::Key(SpecialKey::Enter, mods),
        KeyCode::Backspace => PlannedInput::Key(SpecialKey::Backspace, mods),
        KeyCode::Tab => PlannedInput::Key(SpecialKey::Tab, mods),
        KeyCode::BackTab => PlannedInput::Key(SpecialKey::BackTab, mods),
        KeyCode::Up => PlannedInput::Key(SpecialKey::Up, mods),
        KeyCode::Down => PlannedInput::Key(SpecialKey::Down, mods),
        KeyCode::Left => PlannedInput::Key(SpecialKey::Left, mods),
        KeyCode::Right => PlannedInput::Key(SpecialKey::Right, mods),
        KeyCode::Home => PlannedInput::Key(SpecialKey::Home, mods),
        KeyCode::End => PlannedInput::Key(SpecialKey::End, mods),
        KeyCode::PageUp => PlannedInput::Key(SpecialKey::PageUp, mods),
        KeyCode::PageDown => PlannedInput::Key(SpecialKey::PageDown, mods),
        KeyCode::Insert => PlannedInput::Key(SpecialKey::Insert, mods),
        KeyCode::Delete => PlannedInput::Key(SpecialKey::Delete, mods),
        KeyCode::F(n) => PlannedInput::Key(SpecialKey::F(n), mods),
        // Anything else the engine can't model — treat as consumed no-op
        // by wrapping a Null-equivalent char that the FSM ignores.
        _ => PlannedInput::Key(SpecialKey::Insert, mods),
    }
}

/// One active edit-mode session bound to a single text field.
///
/// Owns the engine [`Editor`] generic over [`BuffrHost`]. The host
/// (clipboard / time / intent fan-out) lives inside the editor as of
/// hjkl 0.1.0 — `Editor<hjkl_buffer::Buffer, BuffrHost>`. Pull-model:
/// per render frame the host calls [`EditSession::take_content_change`]
/// and forwards any new content to the DOM.
pub struct EditSession {
    editor: Editor<hjkl_buffer::Buffer, BuffrHost>,
}

impl EditSession {
    /// Boot the session with the field's current value.
    pub fn new(initial: &str) -> Self {
        let mut editor = Editor::new(
            hjkl_buffer::Buffer::new(),
            BuffrHost::new(),
            Options::default(),
        );
        // 0.1.0: keybinding mode is a post-construction public field on
        // Editor. Vim is the default already, but set explicitly so the
        // intent stays visible at the call site.
        editor.keybinding_mode = KeybindingMode::Vim;
        editor.set_content(initial);
        Self { editor }
    }

    /// Feed one keystroke. Returns `true` when the keystroke was
    /// consumed by the engine; `false` means the caller should let
    /// the page see it (`<Esc>` in normal mode, etc.).
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        self.editor.feed_input(key_event_to_planned(key))
    }

    /// Feed a [`hjkl_engine::PlannedInput`] directly. Bypasses the
    /// crossterm conversion layer — used by the winit event path which
    /// constructs `PlannedInput` from winit's `KeyEvent` directly so
    /// the two crates never need a shared `crossterm` version.
    pub fn feed_planned(&mut self, input: hjkl_engine::PlannedInput) -> bool {
        self.editor.feed_input(input)
    }

    /// Convenience: type a literal character with no modifiers. Used
    /// by tests and by the JS bridge when forwarding plain printable
    /// keys.
    pub fn type_char(&mut self, ch: char) -> bool {
        self.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE))
    }

    /// Convenience: send a special key (Esc, Enter, etc.) with no
    /// modifiers.
    pub fn press(&mut self, code: KeyCode) -> bool {
        self.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    /// Type a string in insert mode. Caller must already have entered
    /// insert mode (`type_char('i')`); panics if asked to type while
    /// not in insert.
    pub fn type_str(&mut self, s: &str) {
        debug_assert_eq!(self.editor.vim_mode(), VimMode::Insert);
        for ch in s.chars() {
            self.type_char(ch);
        }
    }

    /// Pull-model change drain. Returns the new content if anything
    /// changed since the last call; `None` if nothing did. Host
    /// forwards `Some` to the DOM.
    pub fn take_content_change(&mut self) -> Option<Arc<String>> {
        self.editor.take_content_change()
    }

    /// Current full content. Useful for first-frame rendering.
    pub fn content(&self) -> String {
        self.editor.content()
    }

    /// Mode for the status-line summary.
    pub fn vim_mode(&self) -> VimMode {
        self.editor.vim_mode()
    }

    /// Drain queued clipboard writes the engine has accumulated.
    /// Host's tick loop dispatches each to CEF.
    pub fn drain_clipboard_outbox(&mut self) -> Vec<String> {
        self.editor.host_mut().drain_clipboard_outbox()
    }

    /// Drain queued intents (`RequestAutocomplete`, `SwitchBuffer`,
    /// etc.). Host fans each out to its CEF / browser-action layer.
    pub fn drain_intents(&mut self) -> Vec<crate::host::BuffrEditIntent> {
        self.editor.host_mut().drain_intents()
    }

    /// Mutable access to the host. Production callers reach for this
    /// to refresh the clipboard cache on focus events / OSC52 reply.
    pub fn host_mut(&mut self) -> &mut BuffrHost {
        self.editor.host_mut()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_session_starts_in_normal_mode() {
        let s = EditSession::new("");
        assert_eq!(s.vim_mode(), VimMode::Normal);
        // Editor::content() always terminates with a trailing newline.
        assert!(matches!(s.content().as_str(), "" | "\n"));
    }

    #[test]
    fn type_hello_in_insert_then_esc() {
        let mut s = EditSession::new("");
        // `i` enters insert mode.
        s.type_char('i');
        assert_eq!(s.vim_mode(), VimMode::Insert);
        s.type_str("hello");
        s.press(KeyCode::Esc);
        assert_eq!(s.vim_mode(), VimMode::Normal);
        assert!(s.content().starts_with("hello"));
    }

    #[test]
    fn take_content_change_drains_after_first_call() {
        let mut s = EditSession::new("foo");
        // First call returns Some — the initial set_content marked dirty.
        assert!(s.take_content_change().is_some());
        // Subsequent call sees no change.
        assert!(s.take_content_change().is_none());
        // Mutate and confirm the dirty edge re-triggers.
        s.type_char('i');
        s.type_char('X');
        s.press(KeyCode::Esc);
        let after = s.take_content_change();
        assert!(after.is_some());
        assert!(after.unwrap().contains('X'));
    }

    #[test]
    fn dd_clears_only_line() {
        let mut s = EditSession::new("hello world");
        // Two `d` strokes in normal mode delete the line.
        s.type_char('d');
        s.type_char('d');
        // After dd on a one-line buffer, content becomes empty (or just \n).
        let content = s.content();
        assert!(
            content.is_empty() || content == "\n",
            "expected empty or \\n, got {content:?}"
        );
    }

    #[test]
    fn esc_from_normal_stays_normal() {
        // Page-mode would forward an Esc-in-normal back to JS for
        // page-level handling. Here we just confirm the engine
        // doesn't panic and stays in Normal.
        let mut s = EditSession::new("hello");
        s.press(KeyCode::Esc);
        s.press(KeyCode::Esc);
        s.press(KeyCode::Esc);
        assert_eq!(s.vim_mode(), VimMode::Normal);
    }

    #[test]
    fn feed_planned_round_trip() {
        // Stage 2: feed_planned bypasses crossterm; same FSM result.
        // Type `i`, `H`, `i`, Esc → content starts with "Hi".
        use hjkl_engine::{Modifiers, PlannedInput, SpecialKey};
        let empty_mods = Modifiers::default();
        let mut s = EditSession::new("");
        // `i` → enter insert mode
        s.feed_planned(PlannedInput::Char('i', empty_mods));
        assert_eq!(s.vim_mode(), VimMode::Insert);
        s.feed_planned(PlannedInput::Char('H', empty_mods));
        s.feed_planned(PlannedInput::Char('i', empty_mods));
        s.feed_planned(PlannedInput::Key(SpecialKey::Esc, empty_mods));
        assert_eq!(s.vim_mode(), VimMode::Normal);
        assert!(
            s.content().starts_with("Hi"),
            "expected content to start with 'Hi', got {:?}",
            s.content()
        );
    }

    #[test]
    fn yank_then_paste_via_clipboard() {
        // `yy` yanks a line into the unnamed register; `p` pastes it
        // below. Confirms the engine's register pipeline works without
        // the host clipboard plumbing — internal-only.
        let mut s = EditSession::new("alpha");
        s.type_char('y');
        s.type_char('y');
        s.type_char('p');
        let content = s.content();
        // Paste produces "alpha\nalpha" (or similar with a trailing newline).
        assert!(
            content.matches("alpha").count() >= 2,
            "expected two 'alpha' lines, got {content:?}"
        );
    }
}
