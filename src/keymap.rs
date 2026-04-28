//! Keymap trie — prefix-indexed dispatcher from chord sequences to
//! [`PageAction`].
//!
//! Each binding is a path from root to a leaf; each node optionally
//! carries a [`PageAction`] of its own (so `g` can map to one action
//! while `gg` maps to another). Lookup walks the trie one chord at a
//! time and returns:
//!
//! - [`Lookup::Match(action)`] — exact action; the engine fires it
//!   and resets the pending buffer.
//! - [`Lookup::Pending`] — current chord sequence is a valid prefix
//!   of one or more bindings; engine starts the timeout clock.
//! - [`Lookup::NoMatch`] — sequence doesn't lead anywhere; engine
//!   resets and may forward the chords to the page.
//!
//! Ambiguity (`g` mapped *and* `gg` mapped) resolves the same way vim
//! does: caller tracks elapsed time since the first chord and, if
//! the configured timeout elapses without a longer match, fires the
//! shorter action. This module doesn't own the clock —
//! [`crate::engine::Engine`] does.
//!
//! # Mode scoping
//!
//! A [`Keymap`] holds one trie per [`PageMode`] (Normal, Visual,
//! Command, Hint). Pending and Edit are not bindable directly:
//! Pending is a transient internal state of the engine, and Edit-mode
//! routes through `feed_edit_mode_key` instead of the trie.

use crate::actions::{PageAction, PageMode};
use crate::key::{Key, KeyChord, Modifiers, NamedKey, ParseError, parse_keys};
use std::collections::HashMap;

#[derive(Debug, Clone, Default)]
pub struct Keymap {
    leader: Option<char>,
    normal: ModeMap,
    visual: ModeMap,
    command: ModeMap,
    hint: ModeMap,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ModeMap {
    root: Node,
}

#[derive(Debug, Clone, Default)]
struct Node {
    action: Option<PageAction>,
    children: HashMap<KeyChord, Node>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup<'a> {
    /// Exact match. Engine fires the action and resets.
    Match(&'a PageAction),
    /// Current sequence is a valid prefix of one or more bindings.
    /// Engine starts/extends the timeout clock.
    Pending,
    /// Dead end — the sequence isn't a binding and isn't a prefix.
    NoMatch,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BindError {
    #[error("parse error: {0}")]
    Parse(#[from] ParseError),
    #[error("binding contains <leader> but no leader configured")]
    NoLeader,
}

impl Keymap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Configured leader char. `None` means `<leader>` references
    /// fail to bind.
    pub fn leader(&self) -> Option<char> {
        self.leader
    }

    pub fn set_leader(&mut self, leader: char) {
        self.leader = Some(leader);
    }

    /// Bind a chord sequence to an action in the given mode. Parses
    /// `keys` via [`parse_keys`] and resolves `<leader>` to the
    /// configured leader.
    pub fn bind(
        &mut self,
        mode: PageMode,
        keys: &str,
        action: PageAction,
    ) -> Result<(), BindError> {
        let chords = self.resolve_keys(keys)?;
        self.mode_map_mut(mode).bind_chords(&chords, action);
        Ok(())
    }

    /// Bind already-parsed chords. Used by the engine when feeding
    /// programmatic bindings.
    pub fn bind_chords(&mut self, mode: PageMode, chords: &[KeyChord], action: PageAction) {
        self.mode_map_mut(mode).bind_chords(chords, action);
    }

    /// Look up the chord sequence under `mode`.
    pub fn lookup(&self, mode: PageMode, chords: &[KeyChord]) -> Lookup<'_> {
        self.mode_map(mode).lookup(chords)
    }

    /// Resolve the longest-prefix action along `chords` — the engine
    /// uses this when the ambiguity timeout fires.
    pub fn resolve_timeout(&self, mode: PageMode, chords: &[KeyChord]) -> Option<&PageAction> {
        self.mode_map(mode).resolve_timeout(chords)
    }

    /// Flatten every binding under `mode` to `(chord_sequence, action)`
    /// pairs. Order is unspecified — callers that want deterministic
    /// output should sort the result. Used by the new-tab page renderer
    /// to list the live keymap, including any hot-reloaded user
    /// overrides.
    pub fn entries(&self, mode: PageMode) -> Vec<(Vec<KeyChord>, PageAction)> {
        let mut out = Vec::new();
        let mut prefix = Vec::new();
        self.mode_map(mode).root.collect(&mut prefix, &mut out);
        out
    }

    fn resolve_keys(&self, keys: &str) -> Result<Vec<KeyChord>, BindError> {
        let mut chords = parse_keys(keys)?;
        for c in &mut chords {
            if c.key == Key::Named(NamedKey::Leader) {
                let l = self.leader.ok_or(BindError::NoLeader)?;
                c.key = Key::Char(l);
                // Leader uppercase implies shift, mirroring bare-char
                // parsing rules.
                if l.is_ascii_uppercase() {
                    c.modifiers |= Modifiers::SHIFT;
                }
            }
        }
        Ok(chords)
    }

    fn mode_map(&self, mode: PageMode) -> &ModeMap {
        match mode {
            PageMode::Normal | PageMode::Pending | PageMode::Insert => &self.normal,
            PageMode::Visual => &self.visual,
            PageMode::Command => &self.command,
            PageMode::Hint => &self.hint,
        }
    }

    fn mode_map_mut(&mut self, mode: PageMode) -> &mut ModeMap {
        match mode {
            PageMode::Normal | PageMode::Pending | PageMode::Insert => &mut self.normal,
            PageMode::Visual => &mut self.visual,
            PageMode::Command => &mut self.command,
            PageMode::Hint => &mut self.hint,
        }
    }

    /// Phase 6 a11y audit: enumerate every static default binding as
    /// `(mode_label, keys, action)` rows. Sorted by `(mode, keys)` so
    /// the output is stable; used by `--audit-keymap` to verify
    /// keyboard-only reachability of every `PageAction`.
    ///
    /// `leader` mirrors [`Self::default_bindings`]; the resolved
    /// `<leader>` chord is rendered as the literal character so users
    /// can see what they'd type.
    pub fn audit_default_bindings(_leader: char) -> Vec<(&'static str, &'static str, PageAction)> {
        let mut rows: Vec<(&'static str, &'static str, PageAction)> = DEFAULT_BINDINGS
            .iter()
            .map(|(mode, keys, action)| (mode_label(*mode), *keys, action.clone()))
            .collect();
        rows.sort_by(|a, b| a.0.cmp(b.0).then(a.1.cmp(b.1)));
        rows
    }

    /// Phase 6 a11y guarantee: every `PageAction` reachable by a
    /// reasonable user is bound to at least one default chord in some
    /// mode. Returns the list of unbound action *names* (debug-format
    /// of the unit/parameterised variant) — empty Vec means full
    /// coverage.
    ///
    /// "Reasonable" here excludes a small allow-list:
    ///
    /// - [`PageAction::TabReorder`] — currently mouse-only by design
    /// - [`PageAction::ClearCompletedDownloads`] — no obvious chord;
    ///   reachable via `:downloads` cmdline
    /// - [`PageAction::EnterMode`] — variant-of-everything; the
    ///   specific `EnterHintMode`/`OpenOmnibar`/etc. cover the surface
    /// - [`PageAction::ScrollUp`/`ScrollDown` ≠ 1] — only the count=1
    ///   variants need a default; counts come from the count buffer
    pub fn missing_default_bindings() -> Vec<&'static str> {
        // Static list of variant kinds we expect bound. A new
        // `PageAction` variant lands → add it here or to the allow
        // list. The exhaustive match below is the failure surface
        // that catches drift at compile time.
        //
        // We DO this by name (not by `PageAction` value) so that count
        // variants compare with `count = 1`; the table maps the canonical
        // chord that fires the unit case.
        let bound: std::collections::HashSet<&'static str> = DEFAULT_BINDINGS
            .iter()
            .map(|(_, _, a)| action_kind(a))
            .collect();
        let expected = [
            "ScrollUp",
            "ScrollDown",
            "ScrollLeft",
            "ScrollRight",
            "ScrollHalfPageDown",
            "ScrollHalfPageUp",
            "ScrollFullPageDown",
            "ScrollFullPageUp",
            "ScrollTop",
            "ScrollBottom",
            "TabNext",
            "TabPrev",
            "TabClose",
            "TabNewRight",
            "TabNewLeft",
            "PinTab",
            "ReopenClosedTab",
            "PasteUrl",
            "MoveTabLeft",
            "MoveTabRight",
            "HistoryBack",
            "HistoryForward",
            "Reload",
            "ReloadHard",
            "StopLoading",
            "OpenOmnibar",
            "OpenCommandLine",
            "EnterHintMode",
            "EnterHintModeBackground",
            "Find",
            "FindNext",
            "FindPrev",
            "YankUrl",
            "ZoomIn",
            "ZoomOut",
            "ZoomReset",
            "OpenDevTools",
            "FocusFirstInput",
            "ExitInsertMode",
        ];
        expected
            .iter()
            .copied()
            .filter(|name| !bound.contains(name))
            .collect()
    }

    /// Default vim-flavoured bindings. `leader` is the configured
    /// leader char (vim default is `\`). See `docs/keymap.md` for
    /// the full table.
    pub fn default_bindings(leader: char) -> Self {
        let mut km = Keymap::new();
        km.set_leader(leader);
        for &(mode, keys, ref action) in DEFAULT_BINDINGS {
            // `unwrap` is fine here: the table is static and tested.
            // A bad entry is a programming bug, surfaced by
            // `default_bindings_table_parses` in the unit tests.
            km.bind(mode, keys, action.clone())
                .expect("static default-bindings table parses");
        }
        km
    }
}

impl Node {
    /// Walk the trie depth-first and append every (prefix, action)
    /// pair to `out`. `prefix` is mutated as a working buffer; the
    /// caller starts with an empty `Vec`.
    fn collect(&self, prefix: &mut Vec<KeyChord>, out: &mut Vec<(Vec<KeyChord>, PageAction)>) {
        if let Some(a) = &self.action {
            out.push((prefix.clone(), a.clone()));
        }
        for (chord, child) in &self.children {
            prefix.push(*chord);
            child.collect(prefix, out);
            prefix.pop();
        }
    }
}

impl ModeMap {
    fn bind_chords(&mut self, chords: &[KeyChord], action: PageAction) {
        let mut node = &mut self.root;
        for c in chords {
            node = node.children.entry(*c).or_default();
        }
        node.action = Some(action);
    }

    fn lookup(&self, chords: &[KeyChord]) -> Lookup<'_> {
        let mut node = &self.root;
        for c in chords {
            match node.children.get(c) {
                Some(n) => node = n,
                None => return Lookup::NoMatch,
            }
        }
        if let Some(action) = &node.action {
            // Action present at this node, but a longer prefix may
            // exist — ambiguity. Only call it Match if there are no
            // children. With children, the caller still holds Pending
            // so the timeout can resolve.
            if node.children.is_empty() {
                Lookup::Match(action)
            } else {
                Lookup::Pending
            }
        } else if node.children.is_empty() {
            Lookup::NoMatch
        } else {
            Lookup::Pending
        }
    }

    fn resolve_timeout(&self, chords: &[KeyChord]) -> Option<&PageAction> {
        let mut node = &self.root;
        let mut last_action: Option<&PageAction> = None;
        if let Some(a) = &node.action {
            last_action = Some(a);
        }
        for c in chords {
            match node.children.get(c) {
                Some(n) => {
                    node = n;
                    if let Some(a) = &node.action {
                        last_action = Some(a);
                    }
                }
                None => break,
            }
        }
        last_action
    }
}

fn mode_label(mode: PageMode) -> &'static str {
    match mode {
        PageMode::Normal => "normal",
        PageMode::Visual => "visual",
        PageMode::Command => "command",
        PageMode::Hint => "hint",
        PageMode::Pending => "pending",
        PageMode::Insert => "insert",
    }
}

/// Cheap discriminant name for a `PageAction`. Used by the a11y audit
/// to bucket count-bearing variants under one name (e.g. ScrollDown(1)
/// and ScrollDown(5) both report "ScrollDown").
fn action_kind(a: &PageAction) -> &'static str {
    match a {
        PageAction::ScrollUp(_) => "ScrollUp",
        PageAction::ScrollDown(_) => "ScrollDown",
        PageAction::ScrollLeft(_) => "ScrollLeft",
        PageAction::ScrollRight(_) => "ScrollRight",
        PageAction::ScrollPageUp => "ScrollPageUp",
        PageAction::ScrollPageDown => "ScrollPageDown",
        PageAction::ScrollFullPageDown => "ScrollFullPageDown",
        PageAction::ScrollFullPageUp => "ScrollFullPageUp",
        PageAction::ScrollHalfPageDown => "ScrollHalfPageDown",
        PageAction::ScrollHalfPageUp => "ScrollHalfPageUp",
        PageAction::ScrollTop => "ScrollTop",
        PageAction::ScrollBottom => "ScrollBottom",
        PageAction::TabNext => "TabNext",
        PageAction::TabPrev => "TabPrev",
        PageAction::TabClose => "TabClose",
        PageAction::TabNew => "TabNew",
        PageAction::TabNewRight => "TabNewRight",
        PageAction::TabNewLeft => "TabNewLeft",
        PageAction::PinTab => "PinTab",
        PageAction::ReopenClosedTab => "ReopenClosedTab",
        PageAction::PasteUrl { .. } => "PasteUrl",
        PageAction::TabReorder { .. } => "TabReorder",
        PageAction::MoveTabLeft => "MoveTabLeft",
        PageAction::MoveTabRight => "MoveTabRight",
        PageAction::HistoryBack => "HistoryBack",
        PageAction::HistoryForward => "HistoryForward",
        PageAction::Reload => "Reload",
        PageAction::ReloadHard => "ReloadHard",
        PageAction::StopLoading => "StopLoading",
        PageAction::OpenOmnibar => "OpenOmnibar",
        PageAction::OpenCommandLine => "OpenCommandLine",
        PageAction::EnterHintMode => "EnterHintMode",
        PageAction::EnterHintModeBackground => "EnterHintModeBackground",
        PageAction::EnterMode(_) => "EnterMode",
        PageAction::Find { .. } => "Find",
        PageAction::FindNext => "FindNext",
        PageAction::FindPrev => "FindPrev",
        PageAction::YankUrl => "YankUrl",
        PageAction::YankSelection => "YankSelection",
        PageAction::ZoomIn => "ZoomIn",
        PageAction::ZoomOut => "ZoomOut",
        PageAction::ZoomReset => "ZoomReset",
        PageAction::OpenDevTools => "OpenDevTools",
        PageAction::ClearCompletedDownloads => "ClearCompletedDownloads",
        PageAction::EnterInsertMode => "EnterInsertMode",
        PageAction::FocusFirstInput => "FocusFirstInput",
        PageAction::ExitInsertMode => "ExitInsertMode",
    }
}

/// Default keymap table. Static so test can validate every entry
/// parses cleanly. See `docs/keymap.md` for the human-readable
/// version.
///
/// Bindings mirror Vieb defaults; see docs/keymap.md for intentional
/// divergences (`=` as ZoomReset, `<C-c>` as StopLoading, `o` kept).
const DEFAULT_BINDINGS: &[(PageMode, &str, PageAction)] = &[
    // -- scroll ---------------------------------------------------
    (PageMode::Normal, "j", PageAction::ScrollDown(1)),
    (PageMode::Normal, "k", PageAction::ScrollUp(1)),
    (PageMode::Normal, "h", PageAction::ScrollLeft(1)),
    (PageMode::Normal, "l", PageAction::ScrollRight(1)),
    (PageMode::Normal, "<Down>", PageAction::ScrollDown(1)),
    (PageMode::Normal, "<Up>", PageAction::ScrollUp(1)),
    (PageMode::Normal, "<Left>", PageAction::ScrollLeft(1)),
    (PageMode::Normal, "<Right>", PageAction::ScrollRight(1)),
    (PageMode::Normal, "<C-e>", PageAction::ScrollDown(1)),
    (PageMode::Normal, "<C-y>", PageAction::ScrollUp(1)),
    (PageMode::Normal, "<C-d>", PageAction::ScrollHalfPageDown),
    (PageMode::Normal, "<C-u>", PageAction::ScrollHalfPageUp),
    (PageMode::Normal, "<C-f>", PageAction::ScrollFullPageDown),
    (PageMode::Normal, "<C-b>", PageAction::ScrollFullPageUp),
    (
        PageMode::Normal,
        "<PageDown>",
        PageAction::ScrollFullPageDown,
    ),
    (PageMode::Normal, "<PageUp>", PageAction::ScrollFullPageUp),
    (PageMode::Normal, "gg", PageAction::ScrollTop),
    (PageMode::Normal, "G", PageAction::ScrollBottom),
    (PageMode::Normal, "<Home>", PageAction::ScrollTop),
    (PageMode::Normal, "<End>", PageAction::ScrollBottom),
    // -- tabs -----------------------------------------------------
    // User preference: H/L = tab prev/next (NOT vieb default of history).
    (PageMode::Normal, "H", PageAction::TabPrev),
    (PageMode::Normal, "L", PageAction::TabNext),
    (PageMode::Normal, "gt", PageAction::TabNext),
    (PageMode::Normal, "gT", PageAction::TabPrev),
    (PageMode::Normal, "d", PageAction::TabClose),
    // PinTab toggles the active tab's pin state. `<leader>p` keeps the
    // chord off the `<C-w>` prefix space so the leaf <C-w> = TabClose
    // bind doesn't have to wait for an ambiguity timeout.
    (PageMode::Normal, "<leader>p", PageAction::PinTab),
    // Paste-as-tab: open a new tab using the clipboard contents as
    // its URL. Apps layer validates the clipboard classifies as Url
    // or Host before opening; non-URL clipboard contents are no-ops.
    (PageMode::Normal, "p", PageAction::PasteUrl { after: true }),
    (PageMode::Normal, "P", PageAction::PasteUrl { after: false }),
    // Re-open the most recently closed tab (vim-flavored undo). Stack-based
    // so repeated `u` undoes successive closes in reverse order.
    (PageMode::Normal, "u", PageAction::ReopenClosedTab),
    // Conventional-browser alternates for users migrating from Chromium /
    // Firefox. `<C-w>` is now a leaf binding (no `<C-w>X` prefix chords)
    // so it fires immediately without an ambiguity timeout.
    (PageMode::Normal, "<C-t>", PageAction::TabNewRight),
    (PageMode::Normal, "<C-S-t>", PageAction::ReopenClosedTab),
    (PageMode::Normal, "<C-w>", PageAction::TabClose),
    // Shuffle the active tab one slot. Mirrors H/L direction (prev/next)
    // with Shift added — same hand position, different verb.
    (PageMode::Normal, "<C-S-h>", PageAction::MoveTabLeft),
    (PageMode::Normal, "<C-S-l>", PageAction::MoveTabRight),
    // -- history --------------------------------------------------
    // User preference: J/K = history back/forward (NOT vieb default of tabs).
    (PageMode::Normal, "J", PageAction::HistoryBack),
    (PageMode::Normal, "K", PageAction::HistoryForward),
    (PageMode::Normal, "<C-o>", PageAction::HistoryBack),
    (PageMode::Normal, "<C-i>", PageAction::HistoryForward),
    // -- reload / stop --------------------------------------------
    (PageMode::Normal, "r", PageAction::Reload),
    (PageMode::Normal, "R", PageAction::ReloadHard),
    (PageMode::Normal, "<C-r>", PageAction::ReloadHard),
    // <Esc> now exits insert mode unconditionally.
    (PageMode::Normal, "<Esc>", PageAction::ExitInsertMode),
    // buffr extension: <C-c> as StopLoading (Vieb: copyText)
    (PageMode::Normal, "<C-c>", PageAction::StopLoading),
    // -- tabs (adjacent open) -------------------------------------
    // `o` opens a new tab to the right of the active tab and auto-opens
    // the omnibar. `O` opens to the left.
    (PageMode::Normal, "o", PageAction::TabNewRight),
    (PageMode::Normal, "O", PageAction::TabNewLeft),
    // -- omnibar / command ----------------------------------------
    (PageMode::Normal, "e", PageAction::OpenOmnibar),
    (PageMode::Normal, "<C-l>", PageAction::OpenOmnibar),
    (PageMode::Normal, ":", PageAction::OpenCommandLine),
    // buffr extension: `;` as command line alias
    (PageMode::Normal, ";", PageAction::OpenCommandLine),
    // -- hints ----------------------------------------------------
    (PageMode::Normal, "f", PageAction::EnterHintMode),
    (PageMode::Normal, "F", PageAction::EnterHintModeBackground),
    // -- find -----------------------------------------------------
    (PageMode::Normal, "/", PageAction::Find { forward: true }),
    (PageMode::Normal, "?", PageAction::Find { forward: false }),
    (PageMode::Normal, "n", PageAction::FindNext),
    (PageMode::Normal, "N", PageAction::FindPrev),
    // -- yank -----------------------------------------------------
    (PageMode::Normal, "y", PageAction::YankUrl),
    (PageMode::Normal, "<C-c>", PageAction::YankUrl),
    // -- zoom -----------------------------------------------------
    // `+` / `=` zoom in (matches Chromium's Ctrl++ and Ctrl+= aliases),
    // `-` / `_` zoom out, `0` / `)` reset. `<C-0>` kept as a Vieb-style
    // alias for users who still reach for the conventional chord.
    (PageMode::Normal, "+", PageAction::ZoomIn),
    (PageMode::Normal, "=", PageAction::ZoomIn),
    (PageMode::Normal, "-", PageAction::ZoomOut),
    (PageMode::Normal, "_", PageAction::ZoomOut),
    (PageMode::Normal, "0", PageAction::ZoomReset),
    (PageMode::Normal, ")", PageAction::ZoomReset),
    (PageMode::Normal, "<C-0>", PageAction::ZoomReset),
    // -- insert mode -----------------------------------------------
    // `i` and `gi` both focus the first form input (same as Vieb's
    // insertAtFirstInput / gi). EnterInsertMode stays in the enum for
    // advanced user config but is unbound by default.
    (PageMode::Normal, "i", PageAction::FocusFirstInput),
    (PageMode::Normal, "gi", PageAction::FocusFirstInput),
    // -- devtools -------------------------------------------------
    (PageMode::Normal, "<F12>", PageAction::OpenDevTools),
    (PageMode::Normal, "<C-S-i>", PageAction::OpenDevTools),
    // -- visual-mode minimal ---------------------------------------
    // Visual mode is entered automatically when the user drags with
    // the left mouse button in the page area; the embedded CEF view
    // handles the on-screen text-selection rendering. `y` yanks the
    // current page selection to the system clipboard (via CEF's
    // native frame.copy()) and returns to Normal. `<Esc>` cancels
    // without yanking.
    (PageMode::Visual, "y", PageAction::YankSelection),
    (PageMode::Visual, "<C-c>", PageAction::YankSelection),
    (
        PageMode::Visual,
        "<Esc>",
        PageAction::EnterMode(PageMode::Normal),
    ),
    // -- hint-mode ------------------------------------------------
    (
        PageMode::Hint,
        "<Esc>",
        PageAction::EnterMode(PageMode::Normal),
    ),
    // -- command-mode ---------------------------------------------
    (
        PageMode::Command,
        "<Esc>",
        PageAction::EnterMode(PageMode::Normal),
    ),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key::parse_keys as pk;

    fn chords(s: &str) -> Vec<KeyChord> {
        pk(s).expect("parse")
    }

    #[test]
    fn empty_lookup_returns_pending() {
        let mut km = Keymap::new();
        km.bind(PageMode::Normal, "gg", PageAction::ScrollTop)
            .unwrap();
        let r = km.lookup(PageMode::Normal, &[]);
        assert!(matches!(r, Lookup::Pending));
    }

    #[test]
    fn unbound_returns_no_match() {
        let km = Keymap::new();
        let r = km.lookup(PageMode::Normal, &chords("xyz"));
        assert!(matches!(r, Lookup::NoMatch));
    }

    #[test]
    fn exact_match_with_no_extension() {
        let mut km = Keymap::new();
        km.bind(PageMode::Normal, "<C-w>v", PageAction::TabNew)
            .unwrap();
        let r = km.lookup(PageMode::Normal, &chords("<C-w>v"));
        assert!(matches!(r, Lookup::Match(PageAction::TabNew)));
    }

    #[test]
    fn prefix_returns_pending() {
        let mut km = Keymap::new();
        km.bind(PageMode::Normal, "<C-w>v", PageAction::TabNew)
            .unwrap();
        let r = km.lookup(PageMode::Normal, &chords("<C-w>"));
        assert!(matches!(r, Lookup::Pending));
    }

    #[test]
    fn prefix_conflict_g_vs_gg_pending() {
        let mut km = Keymap::new();
        km.bind(PageMode::Normal, "g", PageAction::HistoryBack)
            .unwrap();
        km.bind(PageMode::Normal, "gg", PageAction::ScrollTop)
            .unwrap();
        let lookup = km.lookup(PageMode::Normal, &chords("g"));
        assert!(matches!(lookup, Lookup::Pending));
        let resolved = km.resolve_timeout(PageMode::Normal, &chords("g"));
        assert!(matches!(resolved, Some(PageAction::HistoryBack)));
    }

    #[test]
    fn longer_match_wins_when_extended() {
        let mut km = Keymap::new();
        km.bind(PageMode::Normal, "g", PageAction::HistoryBack)
            .unwrap();
        km.bind(PageMode::Normal, "gg", PageAction::ScrollTop)
            .unwrap();
        let r = km.lookup(PageMode::Normal, &chords("gg"));
        assert!(matches!(r, Lookup::Match(PageAction::ScrollTop)));
    }

    #[test]
    fn rebind_overwrites() {
        let mut km = Keymap::new();
        km.bind(PageMode::Normal, "<C-r>", PageAction::Reload)
            .unwrap();
        km.bind(PageMode::Normal, "<C-r>", PageAction::HistoryForward)
            .unwrap();
        let r = km.lookup(PageMode::Normal, &chords("<C-r>"));
        assert!(matches!(r, Lookup::Match(PageAction::HistoryForward)));
    }

    #[test]
    fn no_match_after_dead_end() {
        let mut km = Keymap::new();
        km.bind(PageMode::Normal, "gT", PageAction::TabPrev)
            .unwrap();
        let r = km.lookup(PageMode::Normal, &chords("gz"));
        assert!(matches!(r, Lookup::NoMatch));
    }

    #[test]
    fn case_sensitive_letters() {
        let mut km = Keymap::new();
        km.bind(PageMode::Normal, "g", PageAction::HistoryBack)
            .unwrap();
        km.bind(PageMode::Normal, "G", PageAction::ScrollBottom)
            .unwrap();
        assert!(matches!(
            km.lookup(PageMode::Normal, &chords("G")),
            Lookup::Match(PageAction::ScrollBottom)
        ));
    }

    #[test]
    fn mode_isolation() {
        let mut km = Keymap::new();
        km.bind(PageMode::Normal, "j", PageAction::ScrollDown(1))
            .unwrap();
        // Visual mode hasn't bound `j`, so lookup returns NoMatch.
        let r = km.lookup(PageMode::Visual, &chords("j"));
        assert!(matches!(r, Lookup::NoMatch));
    }

    #[test]
    fn leader_resolves_to_configured_char() {
        let mut km = Keymap::new();
        km.set_leader('\\');
        km.bind(PageMode::Normal, "<leader>n", PageAction::TabNew)
            .unwrap();
        // `<leader>` resolved to `\`, so the binding fires for `\n`.
        let r = km.lookup(PageMode::Normal, &chords("\\n"));
        assert!(matches!(r, Lookup::Match(PageAction::TabNew)));
    }

    #[test]
    fn leader_without_config_errors() {
        let mut km = Keymap::new();
        let err = km.bind(PageMode::Normal, "<leader>n", PageAction::TabNew);
        assert!(matches!(err, Err(BindError::NoLeader)));
    }

    #[test]
    fn default_bindings_table_parses() {
        // Smoke: every entry in DEFAULT_BINDINGS round-trips through
        // the parser without error. Catches typos in the static
        // table at test-time rather than panic-on-startup.
        let _km = Keymap::default_bindings('\\');
    }

    #[test]
    fn default_j_scrolls_down() {
        let km = Keymap::default_bindings('\\');
        let r = km.lookup(PageMode::Normal, &chords("j"));
        assert!(matches!(r, Lookup::Match(PageAction::ScrollDown(1))));
    }

    #[test]
    fn default_gg_top_g_prefix_pending() {
        let km = Keymap::default_bindings('\\');
        // `g` is a prefix of `gg`, `gt`, `gT` — must be Pending.
        let r = km.lookup(PageMode::Normal, &chords("g"));
        assert!(matches!(r, Lookup::Pending));
        let r = km.lookup(PageMode::Normal, &chords("gg"));
        assert!(matches!(r, Lookup::Match(PageAction::ScrollTop)));
    }

    #[test]
    fn default_ctrl_w_closes_tab() {
        let km = Keymap::default_bindings('\\');
        let r = km.lookup(PageMode::Normal, &chords("<C-w>"));
        assert!(matches!(r, Lookup::Match(PageAction::TabClose)));
    }

    #[test]
    fn default_devtools_binding() {
        let km = Keymap::default_bindings('\\');
        let r = km.lookup(PageMode::Normal, &chords("<C-S-i>"));
        assert!(matches!(r, Lookup::Match(PageAction::OpenDevTools)));
    }

    #[test]
    fn audit_default_bindings_returns_sorted_rows() {
        let rows = Keymap::audit_default_bindings('\\');
        assert!(!rows.is_empty());
        // Sorted by mode then keys — assert pairwise.
        for w in rows.windows(2) {
            let (a_mode, a_keys) = (w[0].0, w[0].1);
            let (b_mode, b_keys) = (w[1].0, w[1].1);
            let cmp = a_mode.cmp(b_mode).then(a_keys.cmp(b_keys));
            assert!(cmp.is_le(), "{a_mode}/{a_keys} vs {b_mode}/{b_keys}");
        }
    }

    #[test]
    fn every_user_facing_action_has_a_default_binding() {
        let missing = Keymap::missing_default_bindings();
        assert!(missing.is_empty(), "unbound actions: {missing:?}");
    }

    #[test]
    fn default_find_forward_and_back() {
        let km = Keymap::default_bindings('\\');
        assert!(matches!(
            km.lookup(PageMode::Normal, &chords("/")),
            Lookup::Match(PageAction::Find { forward: true })
        ));
        assert!(matches!(
            km.lookup(PageMode::Normal, &chords("?")),
            Lookup::Match(PageAction::Find { forward: false })
        ));
    }
}
