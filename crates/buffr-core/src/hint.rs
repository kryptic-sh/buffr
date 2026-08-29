//! Hint mode — DOM-injected overlay labels (Vimium-style follow-by-letter).
//!
//! Architecture: option 2 from `docs/site/ui-stack.md`. We render hints as real
//! DOM elements injected into the page via
//! [`cef::Frame::execute_java_script`]. The hints are absolutely-positioned
//! `<div class="buffr-hint-overlay">` overlays styled in-page and visible
//! because they are part of the page. This avoids the cross-process
//! compositor complexity that would come with an OSR + wgpu overlay path.
//!
//! ## IPC: console-log scraping (fallback path)
//!
//! Communication CEF → Rust uses the **fallback path** documented in the
//! Phase 3 brief: the injected JS calls `console.log("__buffr_hint__:" +
//! nonce + ":" + JSON.stringify(payload))` and our
//! [`crate::handlers::BuffrDisplayHandler`] intercepts those messages via
//! `DisplayHandler::on_console_message`.
//!
//! That callback has no frame argument, so the `nonce` is what tells an
//! authentic event from one forged by the page (or by a third-party iframe
//! on it) — a forged `Ready` event replaces the live [`HintSession`] and
//! turns the user's next hint keystroke into a click on an attacker-chosen
//! element. It is minted per hint session by
//! [`crate::console_nonce::ConsoleNonces::rotate_hint`] and spliced into the
//! asset by [`build_inject_script`]. See [`crate::console_nonce`] for what
//! that does and does not buy.
//!
//! We picked this over `cef_process_message_t` IPC because the message-pipe
//! path requires a renderer-side `RenderProcessHandler` (registered through
//! `CefApp::on_render_process_handler`) plus a V8 binding so JS in the
//! renderer can call `frame->SendProcessMessage(PID_BROWSER, msg)`. That's
//! a meaningful chunk of helper-subprocess plumbing for a slice that only
//! needs a one-way "hint list" message. Console-log scraping reuses the
//! display handler we already have wired and works identically end-to-end.
//!
//! Communication Rust → CEF is the same `execute_java_script` channel: we
//! call `window.__buffrHintFilter(typed)` / `__buffrHintCommit(id)` /
//! `__buffrHintCancel()` from the host.
//!
//! ## Algorithm
//!
//! Greedy-balanced label generation matching Vimium's heuristic:
//!
//! 1. Compute the minimum label length `L = ceil(log_alphabet(N))`.
//! 2. Reserve enough alphabet prefixes to give every element a unique
//!    `L`-length label.
//! 3. When `N < alphabet^L`, distribute shorter labels to prefixes
//!    that don't collide with the reserved set so common targets get
//!    one-character labels.
//!
//! ## Module layout
//!
//! - [`HintAlphabet`] — the configurable alphabet + label generator.
//! - [`HintSession`] — runtime state: typed buffer, current matches.
//! - [`HintAction`] — what `feed()` returns.
//! - [`build_inject_script`] — placeholder substitution for `hint.js`.

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Sentinel that prefixes every hint-mode console message. The display
/// handler scrapes these and routes the JSON tail to a [`HintEventSink`].
pub const HINT_CONSOLE_SENTINEL: &str = "__buffr_hint__:";

/// Hard ceiling on the BFS label queue in
/// [`HintAlphabet::labels_for`]. Purely an OOM backstop: 2^20 labels is
/// four orders of magnitude past any plausible page's interactive-element
/// count, and callers today ask for at most `LABEL_BUDGET`.
const MAX_LABEL_QUEUE: usize = 1 << 20;

/// Default selector list used when the host doesn't pass one. Matches
/// links, buttons, form fields, and anything tagged with an interactive
/// ARIA role or a non-negative tabindex.
pub const DEFAULT_HINT_SELECTORS: &str = "a, button, input, select, textarea, [role=button], [role=link], [role=checkbox], \
     [role=menuitem], [tabindex]:not([tabindex='-1'])";

/// Default alphabet — vim's home-row plus the upper row, mirroring
/// Vimium's defaults. 16 chars → 256 two-letter labels, plenty for
/// dense pages.
pub const DEFAULT_HINT_ALPHABET: &str = "asdfghjkl;weruio";

/// Errors building a [`HintAlphabet`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum HintError {
    #[error("hint alphabet must contain at least 2 distinct characters")]
    AlphabetTooSmall,
    #[error("hint alphabet contains duplicate character: {0:?}")]
    DuplicateChar(char),
}

/// Ordered list of distinct characters used to mint hint labels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HintAlphabet(Vec<char>);

impl HintAlphabet {
    /// Build an alphabet from a string. Whitespace is preserved (so
    /// configs that want literal spaces in labels can have them); the
    /// caller is expected to pass a curated string. The order of the
    /// input is the order in which labels are minted, which matters
    /// for the greedy-balanced algorithm.
    ///
    /// Errors:
    ///
    /// - [`HintError::AlphabetTooSmall`] if fewer than 2 characters.
    /// - [`HintError::DuplicateChar`] on the first repeated codepoint.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(chars: &str) -> Result<Self, HintError> {
        let mut seen = Vec::new();
        for c in chars.chars() {
            if seen.contains(&c) {
                return Err(HintError::DuplicateChar(c));
            }
            seen.push(c);
        }
        if seen.len() < 2 {
            return Err(HintError::AlphabetTooSmall);
        }
        Ok(Self(seen))
    }

    /// Number of distinct characters.
    /// Never 0 — `from_str` rejects alphabets shorter than two chars —
    /// so there is deliberately no `is_empty` (clippy's
    /// `len_without_is_empty` fires; the invariant is enforced at
    /// construction).
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Borrow the alphabet as a slice of characters.
    pub fn chars(&self) -> &[char] {
        &self.0
    }

    /// Render the alphabet back as a `String` (round-trips with
    /// `from_str`). Used when emitting the JS placeholder.
    pub fn as_string(&self) -> String {
        self.0.iter().collect()
    }

    /// Generate exactly `count` labels using the greedy-balanced
    /// Vimium algorithm.
    ///
    /// Behaviour at small `N`:
    ///
    /// - `count == 0` → empty `Vec`.
    /// - `count == 1` → `["a"]` (the first alphabet char).
    /// - `count == alphabet_len` → every char is a one-letter label.
    /// - `count == alphabet_len + 1` → labels grow to two letters; the
    ///   later positions get two-char prefixes so no two labels share
    ///   a prefix.
    ///
    /// Algorithm (canonical Vimium hud.js port):
    ///
    /// 1. Seed a queue with the empty string.
    /// 2. Repeatedly pop the head, push every `c + head` for `c` in the
    ///    alphabet (reversed order keeps the lex sort cheap later).
    /// 3. Stop once `queue.len() - offset >= count`.
    /// 4. Take `count` strings starting at the offset, sort, then
    ///    reverse each (Vimium prepends so the BFS keys are reversed
    ///    relative to the actual label).
    ///
    /// This guarantees:
    ///
    /// - Every label is unique.
    /// - No label is a prefix of another (the queue grows by full
    ///   levels, and we slice from the offset onward).
    /// - Short labels go to the *first* enumerated targets — which is
    ///   what Vimium's "prefix-shorter" feel surfaces.
    pub fn labels_for(&self, count: usize) -> Vec<String> {
        if count == 0 {
            return Vec::new();
        }
        let alpha_len = self.0.len();
        if alpha_len < 2 {
            // `from_str` already rejects this, but defensive guard so
            // future call sites can't divide-by-zero inside the loop.
            return Vec::new();
        }
        // Small-N fast path: when `count <= alpha_len` every label is a
        // single alphabet char, in alphabet order. Skips the BFS +
        // sort below.
        if count <= alpha_len {
            return self.0.iter().take(count).map(|c| c.to_string()).collect();
        }

        // BFS queue. Seed with the empty string so the first
        // expansion produces every single-character label. We track
        // an `offset`: the queue grows monotonically, and entries at
        // `[offset..]` are the as-yet-unexpanded labels — which is also
        // the candidate slice. Once `queue.len() - offset >= count`,
        // the slice is large enough.
        let mut queue: Vec<String> = vec![String::new()];
        let mut offset: usize = 0;
        // Safety cap so a pathological `count` can't OOM us. Must be an
        // absolute number: the old `alpha_len.saturating_pow(16)`
        // saturated to `usize::MAX` for the 16-char default alphabet,
        // so the guard never fired. If we ever hit the cap we still
        // return whatever fit.
        while queue.len() - offset < count && queue.len() < MAX_LABEL_QUEUE {
            // Pop one prefix (BFS head). `mem::take` leaves an empty
            // string at the slot — fine, we'll never read it again.
            let head = std::mem::take(&mut queue[offset]);
            offset += 1;
            for &c in &self.0 {
                let mut s = String::with_capacity(head.len() + c.len_utf8());
                s.push(c);
                s.push_str(&head);
                queue.push(s);
            }
        }

        let take = count.min(queue.len().saturating_sub(offset));
        let mut out: Vec<String> = queue.drain(offset..offset + take).collect();
        // Each entry was prepended (so the BFS keys are reversed
        // relative to the user-visible label). Reverse each first,
        // then sort lex.
        for s in &mut out {
            *s = s.chars().rev().collect();
        }
        out.sort_by(|a, b| label_order(&self.0, a, b));
        out
    }
}

/// Order labels by alphabet position (not Unicode codepoint) so the
/// caller-supplied order in `HintAlphabet` is reflected in the output
/// — "asdf" alphabet ranks `a < s < d < f`. Shorter labels sort before
/// longer ones with the same prefix slot so the first-K targets get
/// one-character labels.
fn label_order(alpha: &[char], a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let mut ai = a.chars();
    let mut bi = b.chars();
    loop {
        match (ai.next(), bi.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(ca), Some(cb)) => {
                let ra = alpha.iter().position(|&c| c == ca).unwrap_or(usize::MAX);
                let rb = alpha.iter().position(|&c| c == cb).unwrap_or(usize::MAX);
                match ra.cmp(&rb) {
                    Ordering::Equal => continue,
                    o => return o,
                }
            }
        }
    }
}

/// Coarse classification of a hint-target element. Comes from the
/// renderer as a string and round-trips through serde so the JSON
/// `kind` field maps directly to this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HintKind {
    Link,
    Button,
    Input,
    Form,
    Other,
}

/// Bounding rectangle for a hint, in CSS pixels relative to the
/// viewport. Informational only — the host never positions anything
/// from these; they're useful in tests and debugging.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HintRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// One hint-target element as reported by the renderer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hint {
    /// Final assigned label (e.g. `"as"`).
    pub label: String,
    /// Renderer-assigned numeric id; round-trips on commit so the JS
    /// can find the right `[data-buffr-hint-target-id="…"]`.
    /// JS posts this as `id`; we keep `element_id` in Rust for clarity.
    #[serde(rename = "id")]
    pub element_id: u32,
    /// Bounding box at the moment of injection. JS flattens `x/y/w/h`
    /// at the same level as `id`/`label`; flatten matches that.
    #[serde(flatten)]
    pub rect: HintRect,
    pub kind: HintKind,
}

// HintAction moved to buffr-engine in Phase 6b (#95) so the BrowserEngine
// trait can reference it without a buffr-core dependency. Re-exported here
// so all existing `buffr_core::HintAction` / `buffr_core::hint::HintAction`
// call sites keep compiling without modification.
pub use buffr_engine::HintAction;

/// Hint-mode runtime state.
///
/// Instances are constructed once the renderer has reported the
/// hint list (via the `ready` console message). The session owns the
/// list of [`Hint`]s and the typed-so-far buffer. It does **not** know
/// about CEF directly — the host calls `feed()` for each keystroke and
/// dispatches the returned [`HintAction`].
#[derive(Debug, Clone)]
pub struct HintSession {
    pub alphabet: HintAlphabet,
    pub hints: Vec<Hint>,
    pub typed: String,
    pub matches: Vec<usize>,
    /// `true` if this session was started from `EnterHintModeBackground`
    /// (`F`). On commit the host emits [`HintAction::OpenInBackground`]
    /// instead of [`HintAction::Click`].
    pub background: bool,
}

impl HintSession {
    /// Build a session from the renderer-reported hint list.
    pub fn new(alphabet: HintAlphabet, hints: Vec<Hint>, background: bool) -> Self {
        let matches: Vec<usize> = (0..hints.len()).collect();
        Self {
            alphabet,
            hints,
            typed: String::new(),
            matches,
            background,
        }
    }

    /// Number of hint targets currently visible (matching `typed`).
    pub fn match_count(&self) -> usize {
        self.matches.len()
    }

    /// Feed one character of user input and decide what the host
    /// should do next.
    ///
    /// Match rules:
    ///
    /// 1. Append `ch` to `typed`.
    /// 2. Filter `matches` to indices whose `label` starts with `typed`.
    /// 3. If exactly one match remains and its `label == typed`,
    ///    return [`HintAction::Click`] / [`HintAction::OpenInBackground`].
    /// 4. If zero remain, return [`HintAction::Cancel`].
    /// 5. Otherwise [`HintAction::Filter`].
    pub fn feed(&mut self, ch: char) -> HintAction {
        self.typed.push(ch);
        self.matches.retain(|&i| {
            self.hints
                .get(i)
                .is_some_and(|h| h.label.starts_with(&self.typed))
        });
        if self.matches.is_empty() {
            return HintAction::Cancel;
        }
        if self.matches.len() == 1 {
            let only = self.matches[0];
            if let Some(h) = self.hints.get(only)
                && h.label == self.typed
            {
                let id = h.element_id;
                return if self.background {
                    HintAction::OpenInBackground(id)
                } else {
                    HintAction::Click(id)
                };
            }
        }
        HintAction::Filter
    }

    /// Backspace pops the last typed char and re-widens the candidate
    /// set. Returns:
    ///
    /// - [`HintAction::Cancel`] when `typed` was already empty
    ///   (caller convention: BS in an unstarted session aborts).
    /// - [`HintAction::Filter`] otherwise — caller calls
    ///   `__buffrHintFilter(typed)` to re-show the previously dimmed
    ///   overlays.
    pub fn backspace(&mut self) -> HintAction {
        if self.typed.is_empty() {
            return HintAction::Cancel;
        }
        self.typed.pop();
        // Re-derive `matches` from scratch so we recover hints
        // dropped by an earlier `feed`.
        self.matches = (0..self.hints.len())
            .filter(|&i| {
                self.hints
                    .get(i)
                    .is_some_and(|h| h.label.starts_with(&self.typed))
            })
            .collect();
        HintAction::Filter
    }
}

/// Renderer-emitted JSON payload variants. The Rust side constructs
/// these from the suffix of a `__buffr_hint__:`-prefixed console line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HintConsoleEvent {
    Ready { hints: Vec<Hint>, alphabet: String },
    Error { message: String },
}

/// A hint event tagged with the browser that emitted it. The handler
/// knows which browser a console line came from and attaches it here so
/// the drain can apply the event to that tab even after the user has
/// switched away (§11-14) — a single-slot untagged sink misrouted one
/// tab's `Ready` to whatever tab happened to be active at drain time.
#[derive(Debug)]
pub struct TaggedHintEvent {
    pub browser_id: i32,
    pub event: HintConsoleEvent,
}

/// One-slot mailbox shared by [`crate::handlers::BuffrDisplayHandler`]
/// and [`crate::host::BrowserHost`]. The display handler writes a
/// [`TaggedHintEvent`] each time the renderer emits a
/// `__buffr_hint__:`-prefixed console line; the host drains the slot
/// from its UI tick.
///
/// One-slot (rather than a queue) because the protocol only has a
/// single "ready" event per session and we'd rather drop a stale
/// duplicate than queue them up.
pub type HintEventSink = Arc<Mutex<Option<TaggedHintEvent>>>;

/// Construct a fresh, empty [`HintEventSink`].
pub fn new_hint_event_sink() -> HintEventSink {
    Arc::new(Mutex::new(None))
}

/// Drain the latest hint event, returning `Some` exactly once per
/// write. Mirrors [`crate::find::take_latest`].
pub fn take_hint_event(sink: &HintEventSink) -> Option<TaggedHintEvent> {
    sink.lock().ok().and_then(|mut guard| guard.take())
}

/// Write a hint event tagged with the emitting browser's id. Only the
/// handler knows which browser a console line came from.
pub fn push_hint_event(sink: &HintEventSink, browser_id: i32, event: HintConsoleEvent) {
    if let Ok(mut guard) = sink.lock() {
        *guard = Some(TaggedHintEvent { browser_id, event });
    }
}

/// Try to parse a console message line as a hint event.
///
/// `nonce` is the hint nonce currently minted for the emitting browser
/// (`ConsoleNonces::hint`).
///
/// Returns `None` when the line is not an authentic hint line for `nonce` —
/// the sentinel is absent, the line is not anchored at the start, or the
/// nonce does not match. Returns `Some(Err(…))` when sentinel *and* nonce
/// matched but the JSON tail won't parse, so callers can log malformed
/// output from our own script without silently dropping it.
pub fn parse_console_event(
    message: &str,
    nonce: &str,
) -> Option<Result<HintConsoleEvent, serde_json::Error>> {
    crate::console_sentinel::parse_sentinel(message, HINT_CONSOLE_SENTINEL, nonce)
}

/// Build the JS payload to send via `frame.execute_java_script`.
///
/// Substitutes the four placeholders the asset uses:
///
/// - `__ALPHABET__`  → the alphabet string, JSON-escaped (so an alphabet
///   containing quotes / non-ASCII doesn't break the JS).
/// - `__LABELS__`    → JSON array of labels (a JS array literal).
/// - `__SELECTORS__` → CSS selectors, JSON-escaped string body.
/// - `%%SENTINEL%%`  → [`HINT_CONSOLE_SENTINEL`] + `nonce` + `:`, the exact
///   prefix [`parse_console_event`] will accept for this session.
///
/// Note the *contents* are JSON-escaped; the placeholders themselves
/// are wrapped in matching quotes inside `hint.js`. We strip the
/// outer quotes that `serde_json::to_string` would produce so the
/// substitution lands inside the existing `'…'` quotes.
///
/// `nonce` comes from [`crate::console_nonce::ConsoleNonces::rotate_hint`]
/// and is plain hex, so it needs no escaping. Inject the result into a
/// **main frame only** — handing a subframe the nonce would give away the
/// very thing it is there to withhold.
pub fn build_inject_script(
    alphabet: &str,
    labels: &[String],
    selectors: &str,
    nonce: &str,
) -> String {
    let alphabet_lit = json_string_inner(alphabet);
    let selectors_lit = json_string_inner(selectors);
    // Labels become an actual JS array literal (with double-quoted
    // strings). Build the literal hand-rolled so we can force every
    // non-ASCII codepoint into `\uXXXX` escapes (mirrors
    // `json_string_inner` so the spliced JS is pure ASCII).
    let mut labels_lit = String::from("[");
    for (i, label) in labels.iter().enumerate() {
        if i > 0 {
            labels_lit.push(',');
        }
        labels_lit.push('"');
        labels_lit.push_str(&crate::js::escape(label));
        labels_lit.push('"');
    }
    labels_lit.push(']');

    let template = include_str!("../assets/hint.js");
    template
        .replace("__ALPHABET__", &alphabet_lit)
        .replace("__LABELS__", &labels_lit)
        .replace("__SELECTORS__", &selectors_lit)
        .replace(
            "%%SENTINEL%%",
            &crate::console_sentinel::sentinel_prefix(HINT_CONSOLE_SENTINEL, nonce),
        )
}

/// JSON-escape `s`, force every non-ASCII codepoint to `\uXXXX`, and
/// strip the surrounding quotes — the asset already wraps the
/// placeholder in `'...'`, and we want the body to be safe to drop in
/// regardless of the source charset.
///
/// We don't trust serde_json's default Unicode pass-through here: the
/// injected JS lives in a `frame.execute_java_script` call where
/// non-ASCII bytes go through CEF's UTF-8 path uninspected, which is
/// fine for valid alphabets but defeats the spec's "ASCII-only,
/// regardless of input" guarantee. Escape manually so the JS string
/// literal is always pure ASCII.
///
/// Shared implementation in [`crate::js::escape`] (A-T1) — it escapes
/// both quote characters, and `\"` inside the single-quoted splice
/// evaluates to a bare `"`, so the extra escaping is harmless.
fn json_string_inner(s: &str) -> String {
    crate::js::escape(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alpha(s: &str) -> HintAlphabet {
        HintAlphabet::from_str(s).expect("alphabet")
    }

    // ---- HintAlphabet --------------------------------------------------

    #[test]
    fn alphabet_rejects_empty() {
        assert_eq!(HintAlphabet::from_str(""), Err(HintError::AlphabetTooSmall));
    }

    #[test]
    fn default_hint_alphabet_matches_config() {
        // `HintConfig::default` cannot import the core const (config →
        // core dep cycle) and duplicates the literal; this pins the two
        // copies together so a drift fails here rather than shipping a
        // label set the config silently overrides.
        assert_eq!(
            DEFAULT_HINT_ALPHABET,
            buffr_config::HintConfig::default().alphabet
        );
    }

    #[test]
    fn alphabet_rejects_single_char() {
        assert_eq!(
            HintAlphabet::from_str("a"),
            Err(HintError::AlphabetTooSmall)
        );
    }

    #[test]
    fn alphabet_rejects_duplicate() {
        assert_eq!(
            HintAlphabet::from_str("abca"),
            Err(HintError::DuplicateChar('a'))
        );
    }

    #[test]
    fn alphabet_accepts_default() {
        let a = HintAlphabet::from_str(DEFAULT_HINT_ALPHABET).unwrap();
        assert_eq!(a.len(), DEFAULT_HINT_ALPHABET.chars().count());
    }

    #[test]
    fn alphabet_round_trips() {
        let a = alpha("asdf");
        assert_eq!(a.as_string(), "asdf");
    }

    #[test]
    fn alphabet_handles_unicode() {
        let a = HintAlphabet::from_str("αβγδ").unwrap();
        assert_eq!(a.len(), 4);
        let labels = a.labels_for(2);
        assert_eq!(labels, vec!["α".to_string(), "β".to_string()]);
    }

    // ---- labels_for boundaries ----------------------------------------

    #[test]
    fn labels_zero() {
        assert_eq!(alpha("asdf").labels_for(0), Vec::<String>::new());
    }

    #[test]
    fn labels_one() {
        assert_eq!(alpha("asdf").labels_for(1), vec!["a".to_string()]);
    }

    #[test]
    fn labels_alphabet_len_minus_one() {
        let a = alpha("asdf");
        assert_eq!(a.labels_for(3), vec!["a", "s", "d"]);
    }

    #[test]
    fn labels_exact_alphabet_len() {
        let a = alpha("asdf");
        assert_eq!(a.labels_for(4), vec!["a", "s", "d", "f"]);
    }

    #[test]
    fn labels_alphabet_len_plus_one() {
        let a = alpha("asdf");
        let labels = a.labels_for(5);
        assert_eq!(labels.len(), 5);
        // No collisions, no prefixes-of-each-other.
        assert_no_prefix_collisions(&labels);
    }

    #[test]
    fn labels_alphabet_squared_minus_one() {
        let a = alpha("asdf"); // 4^2 = 16
        let labels = a.labels_for(15);
        assert_eq!(labels.len(), 15);
        assert_no_prefix_collisions(&labels);
        for l in &labels {
            assert!(l.len() <= 2);
        }
    }

    #[test]
    fn labels_alphabet_squared() {
        let a = alpha("asdf"); // 4^2 = 16
        let labels = a.labels_for(16);
        assert_eq!(labels.len(), 16);
        assert_no_prefix_collisions(&labels);
        // All two-char labels at this size.
        for l in &labels {
            assert_eq!(l.len(), 2, "{l}");
        }
    }

    #[test]
    fn labels_alphabet_squared_plus_one() {
        let a = alpha("asdf"); // 4^2 = 16
        let labels = a.labels_for(17);
        assert_eq!(labels.len(), 17);
        assert_no_prefix_collisions(&labels);
    }

    #[test]
    fn labels_unique() {
        let a = alpha(DEFAULT_HINT_ALPHABET);
        let labels = a.labels_for(200);
        let unique: std::collections::HashSet<_> = labels.iter().collect();
        assert_eq!(unique.len(), labels.len(), "duplicates in {labels:?}");
    }

    #[test]
    fn labels_no_prefix_collisions_default() {
        let a = alpha(DEFAULT_HINT_ALPHABET);
        for &n in &[1usize, 2, 16, 17, 100, 256] {
            let labels = a.labels_for(n);
            assert_eq!(labels.len(), n);
            assert_no_prefix_collisions(&labels);
        }
    }

    /// Regression (L31): the cap used to be `alpha_len.saturating_pow(16)`,
    /// which is `usize::MAX` for the 16-char default alphabet, so the
    /// OOM guard never fired and an absurd `count` allocated until the
    /// process died. The cap must bound the result instead.
    #[test]
    fn labels_for_absurd_count_is_bounded_by_the_cap() {
        let a = alpha(DEFAULT_HINT_ALPHABET);
        let labels = a.labels_for(usize::MAX);
        assert!(!labels.is_empty());
        assert!(
            labels.len() <= MAX_LABEL_QUEUE,
            "cap not enforced: got {}",
            labels.len()
        );
    }

    #[test]
    fn labels_use_alphabet_chars_only() {
        let a = alpha("xyz");
        for label in a.labels_for(20) {
            for c in label.chars() {
                assert!("xyz".contains(c), "stray char {c} in {label}");
            }
        }
    }

    #[test]
    fn labels_minimum_length_grows_with_n() {
        let a = alpha("ab"); // 2-char alphabet — fast growth.
        // 1..=2 → length 1
        for n in 1..=2 {
            for l in a.labels_for(n) {
                assert_eq!(l.len(), 1);
            }
        }
        // 3..=4 → at least one length-2.
        let l3 = a.labels_for(3);
        assert!(l3.iter().any(|s| s.len() >= 2));
        // 5+ → at least one length-3.
        let l5 = a.labels_for(5);
        assert!(l5.iter().any(|s| s.len() >= 3));
    }

    fn assert_no_prefix_collisions(labels: &[String]) {
        for (i, a) in labels.iter().enumerate() {
            for (j, b) in labels.iter().enumerate() {
                if i == j {
                    continue;
                }
                assert!(
                    !b.starts_with(a),
                    "label {a:?} is a prefix of {b:?} (idx {i} vs {j}): full = {labels:?}",
                );
            }
        }
    }

    // ---- HintSession --------------------------------------------------

    fn mk_hints(labels: &[&str]) -> Vec<Hint> {
        labels
            .iter()
            .enumerate()
            .map(|(i, l)| Hint {
                label: (*l).to_string(),
                element_id: i as u32,
                rect: HintRect {
                    x: 0,
                    y: 0,
                    w: 1,
                    h: 1,
                },
                kind: HintKind::Link,
            })
            .collect()
    }

    #[test]
    fn session_filter_narrows_matches() {
        let mut s = HintSession::new(
            alpha(DEFAULT_HINT_ALPHABET),
            mk_hints(&["aa", "ab", "bb"]),
            false,
        );
        let r = s.feed('a');
        assert_eq!(r, HintAction::Filter);
        assert_eq!(s.match_count(), 2);
    }

    #[test]
    fn session_filter_to_one_no_exact_match() {
        let mut s = HintSession::new(alpha(DEFAULT_HINT_ALPHABET), mk_hints(&["aa", "ab"]), false);
        let r = s.feed('a');
        // typed = "a"; both still match; not Click.
        assert_eq!(r, HintAction::Filter);
    }

    #[test]
    fn session_no_match_cancels() {
        let mut s = HintSession::new(alpha(DEFAULT_HINT_ALPHABET), mk_hints(&["aa", "ab"]), false);
        let r = s.feed('z');
        assert_eq!(r, HintAction::Cancel);
        assert_eq!(s.match_count(), 0);
    }

    #[test]
    fn session_exact_match_emits_click() {
        let mut s = HintSession::new(alpha(DEFAULT_HINT_ALPHABET), mk_hints(&["a", "ba"]), false);
        let r = s.feed('a');
        assert_eq!(r, HintAction::Click(0));
    }

    #[test]
    fn session_exact_match_background_emits_open() {
        let mut s = HintSession::new(alpha(DEFAULT_HINT_ALPHABET), mk_hints(&["a", "ba"]), true);
        let r = s.feed('a');
        assert_eq!(r, HintAction::OpenInBackground(0));
    }

    #[test]
    fn session_two_step_commit() {
        let mut s = HintSession::new(
            alpha(DEFAULT_HINT_ALPHABET),
            mk_hints(&["aa", "ab", "ba"]),
            false,
        );
        let r1 = s.feed('a');
        assert_eq!(r1, HintAction::Filter);
        assert_eq!(s.match_count(), 2);
        let r2 = s.feed('b');
        // Now matches {ab} only and label == "ab" == typed.
        assert_eq!(r2, HintAction::Click(1));
    }

    #[test]
    fn session_partial_then_dead_end_cancels() {
        let mut s = HintSession::new(alpha(DEFAULT_HINT_ALPHABET), mk_hints(&["aa", "ab"]), false);
        assert_eq!(s.feed('a'), HintAction::Filter);
        assert_eq!(s.feed('z'), HintAction::Cancel);
    }

    #[test]
    fn session_match_count_starts_full() {
        let s = HintSession::new(
            alpha(DEFAULT_HINT_ALPHABET),
            mk_hints(&["aa", "ab", "bb"]),
            false,
        );
        assert_eq!(s.match_count(), 3);
    }

    #[test]
    fn session_filter_keeps_typed_buffer() {
        let mut s = HintSession::new(alpha(DEFAULT_HINT_ALPHABET), mk_hints(&["asdf"]), false);
        s.feed('a');
        s.feed('s');
        assert_eq!(s.typed, "as");
    }

    #[test]
    fn session_unique_label_after_filter_clicks() {
        // After typing 'a', only "ab" remains; one more 'b' completes.
        let mut s = HintSession::new(
            alpha(DEFAULT_HINT_ALPHABET),
            mk_hints(&["ab", "cd", "ef"]),
            false,
        );
        assert_eq!(s.feed('a'), HintAction::Filter);
        assert_eq!(s.feed('b'), HintAction::Click(0));
    }

    #[test]
    fn session_backspace_empty_cancels() {
        let mut s = HintSession::new(alpha("asdf"), mk_hints(&["aa", "bb"]), false);
        assert_eq!(s.backspace(), HintAction::Cancel);
    }

    #[test]
    fn session_backspace_pops_typed() {
        let mut s = HintSession::new(alpha("asdf"), mk_hints(&["aa", "ab", "bb"]), false);
        s.feed('a');
        assert_eq!(s.match_count(), 2);
        let r = s.backspace();
        assert_eq!(r, HintAction::Filter);
        assert_eq!(s.typed, "");
        assert_eq!(s.match_count(), 3);
    }

    #[test]
    fn session_backspace_recovers_dropped_matches() {
        // Type 'a' then 'z' (Cancel) — backspace re-widens to all 'a*'.
        let mut s = HintSession::new(alpha("asdf"), mk_hints(&["ab", "ac", "bb"]), false);
        s.feed('a');
        // Don't let `feed` set zero matches before we test backspace
        // recovery — type a still-valid char.
        s.feed('b');
        // Now matches just "ab".
        let r = s.backspace();
        assert_eq!(r, HintAction::Filter);
        assert_eq!(s.typed, "a");
        assert_eq!(s.match_count(), 2);
    }

    // ---- console-event parsing ---------------------------------------

    const NONCE: &str = "0123456789abcdef0123456789abcdef";

    fn wire(body: &str) -> String {
        format!("{HINT_CONSOLE_SENTINEL}{NONCE}:{body}")
    }

    #[test]
    fn parse_console_event_ignores_non_sentinel() {
        assert!(parse_console_event("hello world", NONCE).is_none());
    }

    #[test]
    fn parse_console_event_ready() {
        let line = wire(r#"{"kind":"ready","hints":[],"alphabet":"asdf"}"#);
        let ev = parse_console_event(&line, NONCE).unwrap().unwrap();
        match ev {
            HintConsoleEvent::Ready { alphabet, hints } => {
                assert_eq!(alphabet, "asdf");
                assert!(hints.is_empty());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parse_console_event_error() {
        let line = wire(r#"{"kind":"error","message":"boom"}"#);
        let ev = parse_console_event(&line, NONCE).unwrap().unwrap();
        match ev {
            HintConsoleEvent::Error { message } => assert_eq!(message, "boom"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parse_console_event_malformed_returns_inner_err() {
        let line = wire("not json");
        let parsed = parse_console_event(&line, NONCE).unwrap();
        assert!(parsed.is_err());
    }

    #[test]
    fn hint_sink_round_trips_the_browser_tag() {
        // The §11-14 fix rides on the tag surviving push → take; the
        // host-side routing to the emitting tab is the other half.
        let sink = new_hint_event_sink();
        let event = HintConsoleEvent::Ready {
            hints: Vec::new(),
            alphabet: "asdf".to_string(),
        };
        push_hint_event(&sink, 7, event);
        let taken = take_hint_event(&sink).expect("one event pushed");
        assert_eq!(taken.browser_id, 7);
        assert!(matches!(
            taken.event,
            HintConsoleEvent::Ready { hints, .. } if hints.is_empty()
        ));
        // Single-slot: a second push overwrites, and the drain sees it once.
        push_hint_event(
            &sink,
            8,
            HintConsoleEvent::Error {
                message: "x".into(),
            },
        );
        let second = take_hint_event(&sink).unwrap();
        assert_eq!(second.browser_id, 8);
        assert!(take_hint_event(&sink).is_none());
    }

    // ---- H5: forged hint events ---------------------------------------

    #[test]
    fn parse_console_event_rejects_line_without_nonce() {
        // The pre-nonce wire format: any frame could emit this and take
        // over the live HintSession.
        let forged = r#"__buffr_hint__:{"kind":"ready","hints":[],"alphabet":"asdf"}"#;
        assert!(parse_console_event(forged, NONCE).is_none());
    }

    #[test]
    fn parse_console_event_rejects_wrong_nonce() {
        let forged = format!(
            "{HINT_CONSOLE_SENTINEL}{}:{}",
            "f".repeat(32),
            r#"{"kind":"ready","hints":[],"alphabet":"asdf"}"#
        );
        assert!(parse_console_event(&forged, NONCE).is_none());
    }

    #[test]
    fn parse_console_event_rejects_unanchored_sentinel() {
        let forged = format!("%cINFO {}", wire(r#"{"kind":"error","message":"x"}"#));
        assert!(parse_console_event(&forged, NONCE).is_none());
    }

    #[test]
    fn parse_console_event_rejects_nonce_from_another_session() {
        use crate::console_nonce::ConsoleNonces;
        let nonces = ConsoleNonces::new();
        let old = nonces.rotate_hint(1);
        let line = format!(
            "{HINT_CONSOLE_SENTINEL}{old}:{}",
            r#"{"kind":"ready","hints":[],"alphabet":"asdf"}"#
        );
        assert!(parse_console_event(&line, &old).is_some(), "sanity");
        let new = nonces.rotate_hint(1);
        assert!(
            parse_console_event(&line, &new).is_none(),
            "a nonce leaked in a prior session must not work in the next one"
        );
    }

    // ---- build_inject_script ----------------------------------------

    #[test]
    fn inject_script_substitutes_placeholders() {
        let labels = vec!["a".to_string(), "s".to_string()];
        let s = build_inject_script("asdf", &labels, "a, button", NONCE);
        // Sanity: placeholders are gone.
        assert!(!s.contains("__ALPHABET__"));
        assert!(!s.contains("__LABELS__"));
        assert!(!s.contains("__SELECTORS__"));
        assert!(!s.contains("%%SENTINEL%%"));
        // The labels array literal lands inline.
        assert!(s.contains("[\"a\",\"s\"]"));
    }

    #[test]
    fn inject_script_emits_the_prefix_parse_accepts() {
        let labels = vec!["a".to_string()];
        let s = build_inject_script("asdf", &labels, "div", NONCE);
        let prefix = format!("{HINT_CONSOLE_SENTINEL}{NONCE}:");
        assert!(s.contains(&prefix), "nonce not spliced into hint.js");
        let emitted = format!("{prefix}{}", r#"{"kind":"error","message":"boom"}"#);
        assert!(parse_console_event(&emitted, NONCE).unwrap().is_ok());
    }

    #[test]
    fn inject_script_differs_across_sessions() {
        use crate::console_nonce::new_console_nonce;
        let labels = vec!["a".to_string()];
        let a = build_inject_script("asdf", &labels, "div", &new_console_nonce());
        let b = build_inject_script("asdf", &labels, "div", &new_console_nonce());
        assert_ne!(a, b, "nonce must change across injections");
    }

    #[test]
    fn inject_script_escapes_quotes_and_backslashes() {
        // Alphabet with chars that must be JSON-escaped to be safe to
        // splice into a JS string. Single quote tests the
        // `'-inside-single-quoted-string` path; backslash tests JSON's
        // own escape pass-through.
        let labels = vec!["a".to_string()];
        let s = build_inject_script("a'b\\c", &labels, "div", NONCE);
        // No raw single-quote inside the alphabet placement: must be
        // escaped to `\'`.
        // Find the literal alphabet tail: search for `'a` then check
        // the next two bytes don't break the string.
        assert!(s.contains("\\'b"), "single-quote not escaped");
        assert!(s.contains("\\\\c"), "backslash not escaped (json):\n{s}");
    }

    #[test]
    fn inject_script_handles_unicode_alphabet() {
        let labels = vec!["α".to_string()];
        let s = build_inject_script("αβγδ", &labels, "div", NONCE);
        // serde_json escapes non-ASCII into \uXXXX by default; verify
        // the output is valid ASCII so it can't break the surrounding
        // JS string literal regardless of its quote style.
        assert!(s.is_ascii(), "non-ASCII in injected JS:\n{s}");
    }
}
