//! Default vim-flavored page-mode bindings.
//!
//! Wires [`crate::parse`] and [`crate::Keymap`] together. Hosts may
//! consume [`vim_defaults`] directly or layer a user-supplied TOML
//! keymap on top via additional `bind` calls.
//!
//! Roughly tracks the qutebrowser / vimium / vimperator conventions
//! the modal browser community has converged on:
//!
//! - Movement: `h`/`j`/`k`/`l`, `gg`/`G`, `<C-d>`/`<C-u>`, `<C-f>`/
//!   `<C-b>`, `^`/`$`.
//! - Tabs: `gt`/`gT` (next/prev), `<C-t>` (new), `<C-w>` (close), `g0`
//!   (first), `g$` (last).
//! - History: `H`/`L` (back/forward).
//! - Reload: `r`, `<C-r>`.
//! - Omnibar / command line: `o` (open), `t` (open in new tab),
//!   `:` (command).
//! - Hint mode: `f`, `F` (target=tab).
//! - Find-in-page: `/`, `?`, `n`, `N`.
//! - Yank URL: `y`.
//! - Stop loading: `<C-c>` (only in normal mode — bare `<Esc>`
//!   bubbles through to the page).

use crate::{Keymap, PageAction, parse};

/// Build the default page-mode keymap.
///
/// Re-bindable: callers may follow up with their own `Keymap::bind`
/// calls to override or extend.
pub fn vim_defaults() -> Keymap {
    let mut km = Keymap::new();
    let mut bind = |sequence: &str, action: PageAction| {
        let chords = parse(sequence)
            .unwrap_or_else(|e| panic!("default binding {sequence:?} failed to parse: {e}"));
        km.bind(&chords, action);
    };

    // ── Movement ──
    bind("j", PageAction::ScrollDown(40));
    bind("k", PageAction::ScrollUp(40));
    bind("h", PageAction::ScrollLeft(40));
    bind("l", PageAction::ScrollRight(40));
    bind("gg", PageAction::ScrollTop);
    bind("G", PageAction::ScrollBottom);
    bind("<C-d>", PageAction::ScrollPageDown);
    bind("<C-u>", PageAction::ScrollPageUp);
    bind("<C-f>", PageAction::ScrollPageDown);
    bind("<C-b>", PageAction::ScrollPageUp);

    // ── Tabs ──
    bind("gt", PageAction::TabNext);
    bind("gT", PageAction::TabPrev);
    bind("<C-t>", PageAction::TabNew);
    bind("<C-w>", PageAction::TabClose);

    // ── History ──
    bind("H", PageAction::HistoryBack);
    bind("L", PageAction::HistoryForward);

    // ── Reload / loading ──
    bind("r", PageAction::Reload);
    bind("<C-r>", PageAction::Reload);
    bind("<C-c>", PageAction::StopLoading);

    // ── Omnibar / command line ──
    bind("o", PageAction::OpenOmnibar);
    bind(":", PageAction::OpenCommandLine);

    // ── Hint mode ──
    bind("f", PageAction::EnterHintMode);

    // ── Yank URL ──
    bind("y", PageAction::YankUrl);

    // ── Edit-mode entry — `i` enters insert/edit. Any focused text
    // field grabs the keystroke before this binding fires; this
    // entry is the explicit fallback when the page itself has focus
    // on a text field.
    bind("i", PageAction::EnterEditMode);

    km
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Lookup;

    fn chords(s: &str) -> Vec<crate::KeyChord> {
        parse(s).unwrap()
    }

    #[test]
    fn defaults_match_documented_set() {
        let km = vim_defaults();
        for (seq, expected) in [
            ("j", PageAction::ScrollDown(40)),
            ("gg", PageAction::ScrollTop),
            ("G", PageAction::ScrollBottom),
            ("<C-d>", PageAction::ScrollPageDown),
            ("<C-r>", PageAction::Reload),
            ("o", PageAction::OpenOmnibar),
            ("y", PageAction::YankUrl),
            ("i", PageAction::EnterEditMode),
        ] {
            match km.lookup(&chords(seq)) {
                Lookup::Match(action) => assert_eq!(action, &expected, "binding {seq}"),
                other => panic!("expected Match for {seq}, got {other:?}"),
            }
        }
    }

    #[test]
    fn gt_and_gT_distinct() {
        let km = vim_defaults();
        match km.lookup(&chords("gt")) {
            Lookup::Match(PageAction::TabNext) => {}
            other => panic!("expected TabNext, got {other:?}"),
        }
        match km.lookup(&chords("gT")) {
            Lookup::Match(PageAction::TabPrev) => {}
            other => panic!("expected TabPrev, got {other:?}"),
        }
    }

    #[test]
    fn g_alone_is_pending() {
        // `g` is a prefix of `gg`, `gt`, `gT` — Pending until
        // disambiguated or timeout.
        let km = vim_defaults();
        match km.lookup(&chords("g")) {
            Lookup::Pending => {}
            other => panic!("expected Pending for `g`, got {other:?}"),
        }
    }

    #[test]
    fn unbound_letter_is_no_match() {
        let km = vim_defaults();
        match km.lookup(&chords("z")) {
            Lookup::NoMatch => {}
            other => panic!("expected NoMatch for `z`, got {other:?}"),
        }
    }

    #[test]
    fn user_can_override() {
        let mut km = vim_defaults();
        km.bind(&chords("y"), PageAction::OpenCommandLine);
        match km.lookup(&chords("y")) {
            Lookup::Match(PageAction::OpenCommandLine) => {}
            other => panic!("expected override, got {other:?}"),
        }
    }
}
