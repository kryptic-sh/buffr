//! # Page-mode dispatcher engine.
//!
//! The engine wraps a [`Keymap`] with the runtime state every vim-ish
//! dispatcher needs:
//!
//! - **pending chord buffer** — chords accumulate until the trie
//!   reports `Match`, an ambiguity timeout fires, or a `NoMatch`
//!   resets the buffer.
//! - **count prefix** — leading digits 1-9 (and 0 if it's not a
//!   binding by itself) accumulate into a u32 count attached to the
//!   next count-bearing action. `5j` → `ScrollDown(5)`.
//! - **register prefix** — `"<char>` selects a register and stashes
//!   it on the engine. Phase 2 only captures the state; yank-to-
//!   register wiring lands in Phase 5.
//! - **mode** — current [`PageMode`]. Mode transitions arrive two
//!   ways: implicit (specific actions like `OpenOmnibar` /
//!   `EnterHintMode` / `EnterEditMode` move the engine into the
//!   matching mode after dispatch) and explicit
//!   ([`PageAction::EnterMode`]). The user-config friendly catch-all
//!   `EnterMode` co-exists with the legacy specific actions because
//!   each carries a slightly different semantic for the host (the
//!   omnibar action also opens the omnibar UI; a plain `EnterMode`
//!   only changes mode). Both code paths converge on
//!   [`Engine::set_mode`].
//!
//! # Design choice (mode transitions)
//!
//! Per the brief: "Pick whichever is cleaner; document the choice in
//! a code comment at the top of the new file." Choice: **keep the
//! specific actions, add `EnterMode(PageMode)` for raw transitions,
//! and have the engine auto-transition for any of them**. Rationale:
//! the specific actions carry host-side meaning beyond mode change
//! (open the omnibar UI, paint the hint overlay), so they shouldn't
//! collapse into `EnterMode`. The engine treats both as triggers.
//!
//! # Edit-mode stub
//!
//! When the trie returns `EnterEditMode`, the engine sets `mode =
//! Edit` and *stops processing keys via the trie*. Subsequent keys
//! must go through [`Engine::feed_edit_mode_key`] which currently
//! returns `EditModeStep::PassThrough(chord)`. Real wiring lands
//! when `hjkl_engine::Host` extraction ships upstream.

use crate::actions::{PageAction, PageMode};
use crate::key::{Key, KeyChord, NamedKey};
use crate::keymap::{Keymap, Lookup};
use std::time::Duration;

/// Default ambiguity timeout (vim's `&timeoutlen`).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(1000);

/// Result of feeding one chord to the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Engine consumed the chord but doesn't have a match yet. Caller
    /// should keep feeding.
    Pending,
    /// Engine has an ambiguous prefix — exact action present here,
    /// but a longer binding could still match. If `tick(now)` is
    /// called past `timeout_at` the shorter action fires.
    Ambiguous { timeout_at: Duration },
    /// An action resolved. Engine has reset its pending buffer and
    /// any count/register state attached to it.
    Resolved(PageAction),
    /// Chord didn't extend any binding. Engine reset; caller may
    /// forward the original chord(s) to the page if desired.
    Reject,
    /// Edit-mode is active; the trie was bypassed. Caller should
    /// route subsequent chords through
    /// [`Engine::feed_edit_mode_key`].
    EditModeActive,
}

/// What `feed_edit_mode_key` returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditModeStep {
    /// Engine has no real edit-mode wiring yet — pass the chord back
    /// to the caller, who lets the page see it. Will be replaced
    /// once `hjkl_engine::Host` lands.
    PassThrough(KeyChord),
    /// Edit-mode exited (Esc). Engine has flipped back to Normal.
    Exited,
}

/// Page-mode dispatcher.
#[derive(Debug)]
pub struct Engine {
    keymap: Keymap,
    mode: PageMode,
    /// Pre-edit-mode state — restored on Esc out of Edit. Today
    /// always `Normal` since visual/hint don't enter edit-mode.
    return_mode: PageMode,
    pending: Vec<KeyChord>,
    /// Wall-clock instant the first chord of the pending buffer was
    /// fed. None when buffer empty.
    pending_started: Option<Duration>,
    /// Leading-digit count buffer. `0` means no count specified
    /// (binding gets count 1 unless explicit).
    count: u32,
    /// `"<char>` register selector, set when user typed `"a` etc.
    /// Phase 2 captures only — yank-to-register isn't wired.
    register: Option<char>,
    /// Set true while consuming the char *after* a `"`.
    awaiting_register_char: bool,
    timeout: Duration,
}

impl Engine {
    pub fn new(keymap: Keymap) -> Self {
        Self::with_timeout(keymap, DEFAULT_TIMEOUT)
    }

    pub fn with_timeout(keymap: Keymap, timeout: Duration) -> Self {
        Self {
            keymap,
            mode: PageMode::Normal,
            return_mode: PageMode::Normal,
            pending: Vec::new(),
            pending_started: None,
            count: 0,
            register: None,
            awaiting_register_char: false,
            timeout,
        }
    }

    pub fn keymap(&self) -> &Keymap {
        &self.keymap
    }

    pub fn keymap_mut(&mut self) -> &mut Keymap {
        &mut self.keymap
    }

    pub fn mode(&self) -> PageMode {
        self.mode
    }

    /// Force the mode. Resets the pending buffer.
    pub fn set_mode(&mut self, mode: PageMode) {
        self.mode = mode;
        self.reset_pending();
    }

    /// Current pending chord buffer (for status-line rendering).
    pub fn pending(&self) -> &[KeyChord] {
        &self.pending
    }

    /// Currently captured register, if any.
    pub fn register(&self) -> Option<char> {
        self.register
    }

    /// Currently buffered count (0 = none).
    pub fn count(&self) -> u32 {
        self.count
    }

    /// Configured ambiguity timeout.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Feed one chord. The `now` argument is the current wall-clock
    /// duration since some fixed epoch (the engine never reads the
    /// clock itself; the host owns timekeeping).
    pub fn feed(&mut self, chord: KeyChord, now: Duration) -> Step {
        if matches!(self.mode, PageMode::Edit) {
            return Step::EditModeActive;
        }

        // Register prefix consumes one chord at a time.
        if self.awaiting_register_char {
            self.awaiting_register_char = false;
            if let Key::Char(c) = chord.key {
                self.register = Some(c);
                return Step::Pending;
            }
            // Non-char after `"` — abort register selection.
            self.register = None;
            return Step::Reject;
        }

        // Count and register prefixes only apply in Normal mode and
        // only when no chords are pending.
        if matches!(self.mode, PageMode::Normal | PageMode::Visual) && self.pending.is_empty() {
            // `"` starts register selection.
            if chord.modifiers.is_empty() && chord.key == Key::Char('"') {
                self.awaiting_register_char = true;
                return Step::Pending;
            }
            // Digits 1-9 always start a count. `0` only starts a
            // count if a count is already in progress (vim
            // convention: `0` alone is "go to col 0", which here
            // means it's bindable; `10j` works because `1` started
            // the count already).
            if chord.modifiers.is_empty()
                && let Key::Char(c) = chord.key
                && c.is_ascii_digit()
            {
                let d = (c as u32) - ('0' as u32);
                if self.count > 0 || d != 0 {
                    self.count = self.count.saturating_mul(10).saturating_add(d);
                    return Step::Pending;
                }
            }
        }

        // Trie path.
        self.pending.push(chord);
        if self.pending_started.is_none() {
            self.pending_started = Some(now);
        }
        match self.keymap.lookup(self.mode, &self.pending) {
            Lookup::Match(action) => {
                let action = action.clone();
                let resolved = self.finalise_action(action);
                Step::Resolved(resolved)
            }
            Lookup::Pending => {
                // Distinguish "pure prefix" (no action at this node)
                // from "ambiguous" (action here, longer also
                // available).
                if self
                    .keymap
                    .resolve_timeout(self.mode, &self.pending)
                    .is_some()
                {
                    Step::Ambiguous {
                        timeout_at: self.pending_started.unwrap_or(now) + self.timeout,
                    }
                } else {
                    Step::Pending
                }
            }
            Lookup::NoMatch => {
                self.reset_pending();
                Step::Reject
            }
        }
    }

    /// Tick — fire the longest-prefix action when the ambiguity
    /// timeout has elapsed. Returns `Some(action)` if an action
    /// fired; the engine is reset.
    pub fn tick(&mut self, now: Duration) -> Option<PageAction> {
        let started = self.pending_started?;
        if now < started + self.timeout {
            return None;
        }
        let action = self
            .keymap
            .resolve_timeout(self.mode, &self.pending)
            .cloned()?;
        Some(self.finalise_action(action))
    }

    /// Edit-mode key path. Returns a stub today — once
    /// `hjkl_engine::Host` lands upstream this routes through
    /// `hjkl_editor::Editor`.
    // TODO(phase-2-edit): once hjkl_engine::Host lands, route here
    // through hjkl_editor::Editor.
    pub fn feed_edit_mode_key(&mut self, chord: KeyChord) -> EditModeStep {
        // Esc returns to the pre-edit mode.
        if chord.modifiers.is_empty() && chord.key == Key::Named(NamedKey::Esc) {
            self.mode = self.return_mode;
            return EditModeStep::Exited;
        }
        EditModeStep::PassThrough(chord)
    }

    /// Reset pending buffer + count + register. Mode untouched.
    fn reset_pending(&mut self) {
        self.pending.clear();
        self.pending_started = None;
        self.count = 0;
        self.register = None;
        self.awaiting_register_char = false;
    }

    /// Bake the pending count into a count-bearing action and apply
    /// any mode transition the action implies. Resets pending state.
    fn finalise_action(&mut self, action: PageAction) -> PageAction {
        let count = if self.count == 0 { 1 } else { self.count };
        let action = apply_count(action, count);
        self.apply_implicit_mode(&action);
        self.reset_pending();
        action
    }

    fn apply_implicit_mode(&mut self, action: &PageAction) {
        let new_mode = match action {
            PageAction::OpenOmnibar | PageAction::OpenCommandLine => Some(PageMode::Command),
            PageAction::EnterHintMode | PageAction::EnterHintModeBackground => Some(PageMode::Hint),
            PageAction::EnterEditMode => {
                self.return_mode = self.mode;
                Some(PageMode::Edit)
            }
            PageAction::EnterMode(m) => Some(*m),
            _ => None,
        };
        if let Some(m) = new_mode {
            self.mode = m;
        }
    }
}

/// Replace count-bearing scroll actions' counts with `count`. Other
/// actions are returned unchanged (count silently dropped).
fn apply_count(action: PageAction, count: u32) -> PageAction {
    match action {
        PageAction::ScrollUp(_) => PageAction::ScrollUp(count),
        PageAction::ScrollDown(_) => PageAction::ScrollDown(count),
        PageAction::ScrollLeft(_) => PageAction::ScrollLeft(count),
        PageAction::ScrollRight(_) => PageAction::ScrollRight(count),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key::{parse_key, parse_keys};

    fn engine_with(bindings: &[(PageMode, &str, PageAction)]) -> Engine {
        let mut km = Keymap::new();
        km.set_leader('\\');
        for (mode, keys, action) in bindings {
            km.bind(*mode, keys, action.clone()).unwrap();
        }
        Engine::new(km)
    }

    fn t(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    #[test]
    fn single_chord_resolves_immediately() {
        let mut e = engine_with(&[(PageMode::Normal, "j", PageAction::ScrollDown(1))]);
        let r = e.feed(parse_key("j").unwrap(), t(0));
        assert_eq!(r, Step::Resolved(PageAction::ScrollDown(1)));
    }

    #[test]
    fn count_5j_scrolls_5() {
        let mut e = engine_with(&[(PageMode::Normal, "j", PageAction::ScrollDown(1))]);
        for c in parse_keys("5j").unwrap() {
            let _ = e.feed(c, t(0));
        }
        // The final feed returns Resolved(ScrollDown(5)).
        let mut e = engine_with(&[(PageMode::Normal, "j", PageAction::ScrollDown(1))]);
        let chords = parse_keys("5j").unwrap();
        let r1 = e.feed(chords[0], t(0));
        assert_eq!(r1, Step::Pending); // `5` consumed as count.
        let r2 = e.feed(chords[1], t(0));
        assert_eq!(r2, Step::Resolved(PageAction::ScrollDown(5)));
    }

    #[test]
    fn count_multidigit() {
        let mut e = engine_with(&[(PageMode::Normal, "j", PageAction::ScrollDown(1))]);
        for c in parse_keys("12").unwrap() {
            let r = e.feed(c, t(0));
            assert_eq!(r, Step::Pending);
        }
        let r = e.feed(parse_key("j").unwrap(), t(0));
        assert_eq!(r, Step::Resolved(PageAction::ScrollDown(12)));
    }

    #[test]
    fn no_count_means_one() {
        let mut e = engine_with(&[(PageMode::Normal, "j", PageAction::ScrollDown(1))]);
        let r = e.feed(parse_key("j").unwrap(), t(0));
        assert_eq!(r, Step::Resolved(PageAction::ScrollDown(1)));
    }

    #[test]
    fn zero_alone_is_a_binding_not_a_count() {
        // Bind `0` to ScrollLeft(1) — vim convention for "go to col 0".
        let mut e = engine_with(&[(PageMode::Normal, "0", PageAction::ScrollLeft(1))]);
        let r = e.feed(parse_key("0").unwrap(), t(0));
        assert_eq!(r, Step::Resolved(PageAction::ScrollLeft(1)));
    }

    #[test]
    fn zero_after_digit_continues_count() {
        let mut e = engine_with(&[(PageMode::Normal, "j", PageAction::ScrollDown(1))]);
        let r1 = e.feed(parse_key("1").unwrap(), t(0));
        assert_eq!(r1, Step::Pending);
        let r2 = e.feed(parse_key("0").unwrap(), t(0));
        assert_eq!(r2, Step::Pending);
        let r3 = e.feed(parse_key("j").unwrap(), t(0));
        assert_eq!(r3, Step::Resolved(PageAction::ScrollDown(10)));
    }

    #[test]
    fn ambiguity_resolves_via_tick() {
        let mut e = engine_with(&[
            (PageMode::Normal, "g", PageAction::HistoryBack),
            (PageMode::Normal, "gg", PageAction::ScrollTop),
        ]);
        let r = e.feed(parse_key("g").unwrap(), t(0));
        assert!(matches!(r, Step::Ambiguous { .. }));
        // Before timeout: nothing fires.
        assert_eq!(e.tick(t(500)), None);
        // After timeout: shorter action wins.
        assert_eq!(e.tick(t(2000)), Some(PageAction::HistoryBack));
        // Engine is reset.
        assert!(e.pending().is_empty());
    }

    #[test]
    fn ambiguity_extends_to_longer_match() {
        let mut e = engine_with(&[
            (PageMode::Normal, "g", PageAction::HistoryBack),
            (PageMode::Normal, "gg", PageAction::ScrollTop),
        ]);
        let r1 = e.feed(parse_key("g").unwrap(), t(0));
        assert!(matches!(r1, Step::Ambiguous { .. }));
        // Second `g` within timeout window resolves the longer
        // binding.
        let r2 = e.feed(parse_key("g").unwrap(), t(100));
        assert_eq!(r2, Step::Resolved(PageAction::ScrollTop));
    }

    #[test]
    fn pure_prefix_returns_pending_not_ambiguous() {
        // `<C-w>` is only a prefix (no action at that node), so the
        // engine returns Pending, not Ambiguous.
        let mut e = engine_with(&[(PageMode::Normal, "<C-w>c", PageAction::TabClose)]);
        let r = e.feed(parse_key("<C-w>").unwrap(), t(0));
        assert_eq!(r, Step::Pending);
        let r2 = e.feed(parse_key("c").unwrap(), t(50));
        assert_eq!(r2, Step::Resolved(PageAction::TabClose));
    }

    #[test]
    fn no_match_rejects_and_resets() {
        let mut e = engine_with(&[(PageMode::Normal, "j", PageAction::ScrollDown(1))]);
        let r = e.feed(parse_key("z").unwrap(), t(0));
        assert_eq!(r, Step::Reject);
        assert!(e.pending().is_empty());
        // Engine recovers; next chord works.
        let r2 = e.feed(parse_key("j").unwrap(), t(10));
        assert_eq!(r2, Step::Resolved(PageAction::ScrollDown(1)));
    }

    #[test]
    fn register_quote_a_then_y_captures_state() {
        // Bind `y` to YankUrl. Feed `"ay`: the engine captures
        // register `a` and then resolves YankUrl. Phase 2 contract:
        // register state observable on the engine after the action
        // resolves — the action itself doesn't carry it yet.
        let mut e = engine_with(&[(PageMode::Normal, "y", PageAction::YankUrl)]);
        let r1 = e.feed(parse_key("\"").unwrap(), t(0));
        assert_eq!(r1, Step::Pending);
        let r2 = e.feed(parse_key("a").unwrap(), t(0));
        assert_eq!(r2, Step::Pending);
        // After `"a`, register captured.
        assert_eq!(e.register(), Some('a'));
        let r3 = e.feed(parse_key("y").unwrap(), t(0));
        assert_eq!(r3, Step::Resolved(PageAction::YankUrl));
        // Action resolved; register cleared with the rest of pending state.
        assert_eq!(e.register(), None);
    }

    #[test]
    fn omnibar_action_transitions_mode() {
        let mut e = engine_with(&[(PageMode::Normal, "o", PageAction::OpenOmnibar)]);
        assert_eq!(e.mode(), PageMode::Normal);
        let r = e.feed(parse_key("o").unwrap(), t(0));
        assert_eq!(r, Step::Resolved(PageAction::OpenOmnibar));
        assert_eq!(e.mode(), PageMode::Command);
    }

    #[test]
    fn enter_hint_action_transitions_mode() {
        let mut e = engine_with(&[(PageMode::Normal, "f", PageAction::EnterHintMode)]);
        let r = e.feed(parse_key("f").unwrap(), t(0));
        assert_eq!(r, Step::Resolved(PageAction::EnterHintMode));
        assert_eq!(e.mode(), PageMode::Hint);
    }

    #[test]
    fn enter_mode_explicit_action() {
        let mut e = engine_with(&[(
            PageMode::Normal,
            "v",
            PageAction::EnterMode(PageMode::Visual),
        )]);
        let _ = e.feed(parse_key("v").unwrap(), t(0));
        assert_eq!(e.mode(), PageMode::Visual);
    }

    #[test]
    fn edit_mode_blocks_trie() {
        let mut e = engine_with(&[
            (PageMode::Normal, "i", PageAction::EnterEditMode),
            (PageMode::Normal, "j", PageAction::ScrollDown(1)),
        ]);
        let r = e.feed(parse_key("i").unwrap(), t(0));
        assert_eq!(r, Step::Resolved(PageAction::EnterEditMode));
        assert_eq!(e.mode(), PageMode::Edit);
        // After entering edit-mode the trie is bypassed.
        let r2 = e.feed(parse_key("j").unwrap(), t(0));
        assert_eq!(r2, Step::EditModeActive);
    }

    #[test]
    fn edit_mode_passthrough_then_esc_exits() {
        let mut e = engine_with(&[(PageMode::Normal, "i", PageAction::EnterEditMode)]);
        let _ = e.feed(parse_key("i").unwrap(), t(0));
        assert_eq!(e.mode(), PageMode::Edit);
        let chord = parse_key("a").unwrap();
        assert_eq!(
            e.feed_edit_mode_key(chord),
            EditModeStep::PassThrough(chord)
        );
        assert_eq!(e.mode(), PageMode::Edit);
        let exit = e.feed_edit_mode_key(parse_key("<Esc>").unwrap());
        assert_eq!(exit, EditModeStep::Exited);
        assert_eq!(e.mode(), PageMode::Normal);
    }

    #[test]
    fn count_does_not_apply_to_non_count_actions() {
        let mut e = engine_with(&[(PageMode::Normal, "r", PageAction::Reload)]);
        let _ = e.feed(parse_key("5").unwrap(), t(0));
        let r = e.feed(parse_key("r").unwrap(), t(0));
        // Reload has no count slot — count is silently dropped.
        assert_eq!(r, Step::Resolved(PageAction::Reload));
        assert_eq!(e.count(), 0);
    }

    #[test]
    fn tick_no_pending_returns_none() {
        let mut e = engine_with(&[]);
        assert_eq!(e.tick(t(5000)), None);
    }
}
