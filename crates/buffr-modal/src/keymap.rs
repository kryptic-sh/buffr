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
            PageMode::Normal | PageMode::Pending | PageMode::Edit => &self.normal,
            PageMode::Visual => &self.visual,
            PageMode::Command => &self.command,
            PageMode::Hint => &self.hint,
        }
    }

    fn mode_map_mut(&mut self, mode: PageMode) -> &mut ModeMap {
        match mode {
            PageMode::Normal | PageMode::Pending | PageMode::Edit => &mut self.normal,
            PageMode::Visual => &mut self.visual,
            PageMode::Command => &mut self.command,
            PageMode::Hint => &mut self.hint,
        }
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

/// Default keymap table. Static so test can validate every entry
/// parses cleanly. See `docs/keymap.md` for the human-readable
/// version.
const DEFAULT_BINDINGS: &[(PageMode, &str, PageAction)] = &[
    // -- scroll ---------------------------------------------------
    (PageMode::Normal, "j", PageAction::ScrollDown(1)),
    (PageMode::Normal, "k", PageAction::ScrollUp(1)),
    (PageMode::Normal, "h", PageAction::ScrollLeft(1)),
    (PageMode::Normal, "l", PageAction::ScrollRight(1)),
    (PageMode::Normal, "<C-d>", PageAction::ScrollHalfPageDown),
    (PageMode::Normal, "<C-u>", PageAction::ScrollHalfPageUp),
    (PageMode::Normal, "<C-f>", PageAction::ScrollFullPageDown),
    (PageMode::Normal, "<C-b>", PageAction::ScrollFullPageUp),
    (PageMode::Normal, "gg", PageAction::ScrollTop),
    (PageMode::Normal, "G", PageAction::ScrollBottom),
    // -- tabs -----------------------------------------------------
    (PageMode::Normal, "gt", PageAction::TabNext),
    (PageMode::Normal, "gT", PageAction::TabPrev),
    (PageMode::Normal, "<C-w>c", PageAction::TabClose),
    (PageMode::Normal, "t", PageAction::TabNew),
    (PageMode::Normal, "<C-w>n", PageAction::DuplicateTab),
    (PageMode::Normal, "<C-w>p", PageAction::PinTab),
    // -- history --------------------------------------------------
    (PageMode::Normal, "H", PageAction::HistoryBack),
    (PageMode::Normal, "L", PageAction::HistoryForward),
    // -- reload / stop --------------------------------------------
    (PageMode::Normal, "r", PageAction::Reload),
    (PageMode::Normal, "<C-r>", PageAction::ReloadHard),
    (PageMode::Normal, "<C-c>", PageAction::StopLoading),
    // -- omnibar / command ----------------------------------------
    (PageMode::Normal, "o", PageAction::OpenOmnibar),
    (PageMode::Normal, ":", PageAction::OpenCommandLine),
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
    // -- zoom -----------------------------------------------------
    (PageMode::Normal, "+", PageAction::ZoomIn),
    (PageMode::Normal, "-", PageAction::ZoomOut),
    (PageMode::Normal, "=", PageAction::ZoomReset),
    // -- devtools -------------------------------------------------
    (PageMode::Normal, "<C-S-i>", PageAction::OpenDevTools),
    // -- visual-mode minimal ---------------------------------------
    // Esc returns to normal — the engine handles this via the mode
    // transition code path; the binding here is a placeholder so
    // the visual-mode trie is populated at all.
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
    fn default_ctrl_w_c_closes_tab() {
        let km = Keymap::default_bindings('\\');
        let r = km.lookup(PageMode::Normal, &chords("<C-w>c"));
        assert!(matches!(r, Lookup::Match(PageAction::TabClose)));
    }

    #[test]
    fn default_devtools_binding() {
        let km = Keymap::default_bindings('\\');
        let r = km.lookup(PageMode::Normal, &chords("<C-S-i>"));
        assert!(matches!(r, Lookup::Match(PageAction::OpenDevTools)));
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
