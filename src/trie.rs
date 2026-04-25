//! Keymap trie — prefix-indexed dispatcher from chord sequences to
//! [`crate::PageAction`].
//!
//! Each binding is a path from root to a leaf; each node optionally
//! carries a [`PageAction`] of its own (so `g` can map to one action
//! while `gg` maps to another). Lookup walks the trie one chord at a
//! time and returns:
//!
//! - [`Lookup::Match(action)`] — exact action; the dispatcher fires
//!   it and resets the pending buffer.
//! - [`Lookup::Pending`] — current chord sequence is a valid prefix
//!   of one or more bindings; caller should hold the keystroke
//!   buffer and start the timeout clock.
//! - [`Lookup::NoMatch`] — sequence doesn't lead anywhere; reset and
//!   let the page see the chords if the dispatcher decides to.
//!
//! Ambiguity (`g` mapped *and* `gg` mapped) resolves the same way vim
//! does: caller tracks elapsed time since the first chord and, if
//! `Options::timeout_len` elapses without a longer match, fires the
//! shorter action. This module doesn't own the clock — it only
//! reports `Pending` vs `Match`.

use crate::{KeyChord, PageAction};
use std::collections::HashMap;

#[derive(Debug, Clone, Default)]
pub struct Keymap {
    root: Node,
}

#[derive(Debug, Clone, Default)]
struct Node {
    action: Option<PageAction>,
    children: HashMap<KeyChord, Node>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup<'a> {
    /// Exact match. Dispatcher fires the action and resets.
    Match(&'a PageAction),
    /// Current sequence is a valid prefix of one or more bindings.
    /// Caller starts/extends the timeout clock.
    Pending,
    /// Dead end — the sequence isn't a binding and isn't a prefix.
    NoMatch,
}

impl Keymap {
    pub fn new() -> Self {
        Keymap::default()
    }

    /// Insert a binding from chord sequence to action. Re-binding the
    /// same sequence overwrites the previous action.
    pub fn bind(&mut self, chords: &[KeyChord], action: PageAction) {
        let mut node = &mut self.root;
        for c in chords {
            node = node.children.entry(*c).or_default();
        }
        node.action = Some(action);
    }

    /// Look up a chord sequence. The dispatcher feeds the **full
    /// pending buffer** (not one chord at a time); a `Pending` result
    /// means "wait for more or timeout."
    pub fn lookup(&self, chords: &[KeyChord]) -> Lookup<'_> {
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
            // children (no extension possible). With children, the
            // caller still holds Pending so timeout can resolve.
            if node.children.is_empty() {
                Lookup::Match(action)
            } else {
                Lookup::Pending
            }
        } else if node.children.is_empty() {
            // Empty internal node — shouldn't occur unless a binding
            // was removed; treat as no-match so dispatcher resets.
            Lookup::NoMatch
        } else {
            Lookup::Pending
        }
    }

    /// Resolve the current pending sequence to an action when the
    /// timeout fires. Returns the longest action stored along the
    /// path; `None` if no node on the path bound an action.
    pub fn resolve_timeout(&self, chords: &[KeyChord]) -> Option<&PageAction> {
        let mut node = &self.root;
        let mut last_action: Option<&PageAction> = None;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::parse;

    fn chords(s: &str) -> Vec<KeyChord> {
        parse(s).expect("parse")
    }

    #[test]
    fn empty_lookup_returns_pending() {
        let mut km = Keymap::new();
        km.bind(&chords("gg"), PageAction::ScrollTop);
        // Empty input is at root, which has children → Pending.
        let r = km.lookup(&[]);
        assert!(matches!(r, Lookup::Pending));
    }

    #[test]
    fn unbound_returns_no_match() {
        let km = Keymap::new();
        let r = km.lookup(&chords("xyz"));
        assert!(matches!(r, Lookup::NoMatch));
    }

    #[test]
    fn exact_match_with_no_extension() {
        let mut km = Keymap::new();
        km.bind(&chords("<C-w>v"), PageAction::TabNew);
        let r = km.lookup(&chords("<C-w>v"));
        assert!(matches!(r, Lookup::Match(PageAction::TabNew)));
    }

    #[test]
    fn prefix_returns_pending() {
        let mut km = Keymap::new();
        km.bind(&chords("<C-w>v"), PageAction::TabNew);
        let r = km.lookup(&chords("<C-w>"));
        assert!(matches!(r, Lookup::Pending));
    }

    #[test]
    fn ambiguous_short_path_is_pending_until_timeout() {
        // `g` and `gg` both bound. Lookup of just `g` returns Pending
        // because `gg` extends it; only resolve_timeout fires the
        // shorter action.
        let mut km = Keymap::new();
        km.bind(&chords("g"), PageAction::HistoryBack);
        km.bind(&chords("gg"), PageAction::ScrollTop);

        let lookup = km.lookup(&chords("g"));
        assert!(matches!(lookup, Lookup::Pending));

        let resolved = km.resolve_timeout(&chords("g"));
        assert!(matches!(resolved, Some(PageAction::HistoryBack)));
    }

    #[test]
    fn longer_match_wins_when_extended() {
        let mut km = Keymap::new();
        km.bind(&chords("g"), PageAction::HistoryBack);
        km.bind(&chords("gg"), PageAction::ScrollTop);

        let r = km.lookup(&chords("gg"));
        assert!(matches!(r, Lookup::Match(PageAction::ScrollTop)));
    }

    #[test]
    fn rebind_overwrites() {
        let mut km = Keymap::new();
        km.bind(&chords("<C-r>"), PageAction::Reload);
        km.bind(&chords("<C-r>"), PageAction::HistoryForward);
        let r = km.lookup(&chords("<C-r>"));
        assert!(matches!(r, Lookup::Match(PageAction::HistoryForward)));
    }

    #[test]
    fn no_match_after_dead_end() {
        let mut km = Keymap::new();
        km.bind(&chords("gT"), PageAction::TabPrev);
        // `gz` isn't bound; lookup terminates with NoMatch.
        let r = km.lookup(&chords("gz"));
        assert!(matches!(r, Lookup::NoMatch));
    }

    #[test]
    fn resolve_timeout_returns_longest_seen() {
        let mut km = Keymap::new();
        km.bind(&chords("g"), PageAction::HistoryBack);
        km.bind(&chords("gg"), PageAction::ScrollTop);
        // Walking `gz` enters the `g` node (action HistoryBack) then
        // dead-ends at `z`. Timeout resolution returns HistoryBack.
        let r = km.resolve_timeout(&chords("gz"));
        assert!(matches!(r, Some(PageAction::HistoryBack)));
    }

    #[test]
    fn case_sensitive_letters() {
        let mut km = Keymap::new();
        km.bind(&chords("g"), PageAction::HistoryBack);
        km.bind(&chords("G"), PageAction::ScrollBottom);
        assert!(matches!(
            km.lookup(&chords("g")),
            Lookup::Match(PageAction::HistoryBack) | Lookup::Pending
        ));
        assert!(matches!(
            km.lookup(&chords("G")),
            Lookup::Match(PageAction::ScrollBottom)
        ));
    }
}
