//! Hint mode — DOM-injected overlay labels (Vimium-style follow-by-letter).
//!
//! Architecture: option 2 from `docs/ui-stack.md`. We render hints as real
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
//! JSON.stringify(payload))` and our [`crate::handlers::BuffrDisplayHandler`]
//! intercepts those messages via `DisplayHandler::on_console_message`.
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

/// CSS class applied to every injected hint overlay div. Documented here
/// (not just in the JS asset) so a future user-CSS-block can avoid
/// colliding with it.
pub const HINT_OVERLAY_CLASS: &str = "buffr-hint-overlay";

/// `z-index` used by the injected style for [`HINT_OVERLAY_CLASS`].
/// Maxed out so user-page overlays can't shadow the hints.
pub const HINT_OVERLAY_Z_INDEX: i64 = 2_147_483_647;

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
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the alphabet is empty (it never is — `from_str` rejects
    /// short input — but `Vec`-style is_empty mirrors std for symmetry).
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
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
        // Safety cap so a pathological `count` can't OOM us.
        // alpha_len^16 dwarfs any plausible page; if we ever hit this
        // we still return whatever fit.
        let cap = alpha_len.saturating_pow(16);
        while queue.len() - offset < count && queue.len() < cap {
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

/// What [`HintSession::feed`] returns to the host. Mirrors the result
/// shape from [`buffr_modal::engine::Step`] but for hint-mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HintAction {
    /// User typed a char that narrowed the candidate set but didn't
    /// commit. UI should call `__buffrHintFilter(typed)` to dim the
    /// non-matching overlays.
    Filter,
    /// One label remained and the typed string equals it. Caller
    /// should dispatch the click + exit hint mode.
    Click(u32),
    /// Background variant. Today the host falls back to a regular
    /// click + tracing breadcrumb (multi-tab is Phase 5b).
    OpenInBackground(u32),
    /// User typed a char that no label starts with. Caller should
    /// cancel the hint session.
    Cancel,
}

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

    /// Esc always cancels the session.
    pub fn esc(&mut self) -> HintAction {
        HintAction::Cancel
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

/// Alias for [`Hint`] — the `HintLabel` name is used in some external
/// docs / specs. They're the same type.
pub type HintLabel = Hint;

/// Renderer-emitted JSON payload variants. The Rust side constructs
/// these from the suffix of a `__buffr_hint__:`-prefixed console line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HintConsoleEvent {
    Ready { hints: Vec<Hint>, alphabet: String },
    Error { message: String },
}

/// One-slot mailbox shared by [`crate::handlers::BuffrDisplayHandler`]
/// and [`crate::host::BrowserHost`]. The display handler writes a
/// parsed [`HintConsoleEvent`] each time the renderer emits a
/// `__buffr_hint__:`-prefixed console line; the host drains the slot
/// from its UI tick.
///
/// One-slot (rather than a queue) because the protocol only has a
/// single "ready" event per session and we'd rather drop a stale
/// duplicate than queue them up.
pub type HintEventSink = Arc<Mutex<Option<HintConsoleEvent>>>;

/// Construct a fresh, empty [`HintEventSink`].
pub fn new_hint_event_sink() -> HintEventSink {
    Arc::new(Mutex::new(None))
}

/// Drain the latest hint event, returning `Some` exactly once per
/// write. Mirrors [`crate::find::take_latest`].
pub fn take_hint_event(sink: &HintEventSink) -> Option<HintConsoleEvent> {
    sink.lock().ok().and_then(|mut guard| guard.take())
}

/// Try to parse a console message line as a hint event. Returns `None`
/// when the line doesn't carry the sentinel prefix. Returns
/// `Some(Err(…))` when the prefix is present but the JSON tail won't
/// parse — useful so callers can log malformed renderer output without
/// silently dropping it.
pub fn parse_console_event(message: &str) -> Option<Result<HintConsoleEvent, serde_json::Error>> {
    // Find the sentinel anywhere in the line — some sites wrap
    // `console.log` to prepend styling format strings.
    let idx = message.find(HINT_CONSOLE_SENTINEL)?;
    let suffix = &message[idx + HINT_CONSOLE_SENTINEL.len()..];
    Some(serde_json::from_str::<HintConsoleEvent>(suffix))
}

/// Build the JS payload to send via `frame.execute_java_script`.
///
/// Substitutes the three placeholders the asset uses:
///
/// - `__ALPHABET__`  → the alphabet string, JSON-escaped (so an alphabet
///   containing quotes / non-ASCII doesn't break the JS).
/// - `__LABELS__`    → JSON array of labels (a JS array literal).
/// - `__SELECTORS__` → CSS selectors, JSON-escaped string body.
///
/// Note the *contents* are JSON-escaped; the placeholders themselves
/// are wrapped in matching quotes inside `hint.js`. We strip the
/// outer quotes that `serde_json::to_string` would produce so the
/// substitution lands inside the existing `'…'` quotes.
pub fn build_inject_script(alphabet: &str, labels: &[String], selectors: &str) -> String {
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
        for c in label.chars() {
            match c {
                '"' => labels_lit.push_str("\\\""),
                '\\' => labels_lit.push_str("\\\\"),
                '\n' => labels_lit.push_str("\\n"),
                '\r' => labels_lit.push_str("\\r"),
                '\t' => labels_lit.push_str("\\t"),
                c if c.is_ascii_graphic() || c == ' ' => labels_lit.push(c),
                c => {
                    let mut buf = [0u16; 2];
                    for unit in c.encode_utf16(&mut buf).iter() {
                        labels_lit.push_str(&format!("\\u{unit:04x}"));
                    }
                }
            }
        }
        labels_lit.push('"');
    }
    labels_lit.push(']');

    let template = include_str!("../assets/hint.js");
    template
        .replace("__ALPHABET__", &alphabet_lit)
        .replace("__LABELS__", &labels_lit)
        .replace("__SELECTORS__", &selectors_lit)
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
fn json_string_inner(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            // Single-quote: the asset uses `'...'`, escape so the
            // splice can't terminate the literal.
            '\'' => out.push_str("\\'"),
            // Backslash: belt-and-braces — serde_json would have
            // escaped these but we're hand-rolling.
            '\\' => out.push_str("\\\\"),
            // Embedded newline / CR / tab → JS escapes.
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // Plain printable ASCII passes through.
            c if c.is_ascii_graphic() || c == ' ' => out.push(c),
            // Everything else (including double quotes inside JS
            // strings, control chars, and non-ASCII): emit \uXXXX
            // surrogate pairs for codepoints above the BMP.
            c => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf).iter() {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
        }
    }
    out
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
    fn session_esc_cancels_anytime() {
        let mut s = HintSession::new(alpha("asdf"), mk_hints(&["a", "b"]), false);
        assert_eq!(s.esc(), HintAction::Cancel);
        // Re-issuing esc after a feed still returns Cancel.
        s.feed('a');
        assert_eq!(s.esc(), HintAction::Cancel);
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

    #[test]
    fn parse_console_event_ignores_non_sentinel() {
        assert!(parse_console_event("hello world").is_none());
    }

    #[test]
    fn parse_console_event_ready() {
        let line = r#"__buffr_hint__:{"kind":"ready","hints":[],"alphabet":"asdf"}"#;
        let ev = parse_console_event(line).unwrap().unwrap();
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
        let line = r#"__buffr_hint__:{"kind":"error","message":"boom"}"#;
        let ev = parse_console_event(line).unwrap().unwrap();
        match ev {
            HintConsoleEvent::Error { message } => assert_eq!(message, "boom"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parse_console_event_malformed_returns_inner_err() {
        let line = "__buffr_hint__:not json";
        let parsed = parse_console_event(line).unwrap();
        assert!(parsed.is_err());
    }

    // ---- build_inject_script ----------------------------------------

    #[test]
    fn inject_script_substitutes_placeholders() {
        let labels = vec!["a".to_string(), "s".to_string()];
        let s = build_inject_script("asdf", &labels, "a, button");
        // Sanity: placeholders are gone.
        assert!(!s.contains("__ALPHABET__"));
        assert!(!s.contains("__LABELS__"));
        assert!(!s.contains("__SELECTORS__"));
        // The labels array literal lands inline.
        assert!(s.contains("[\"a\",\"s\"]"));
    }

    #[test]
    fn inject_script_escapes_quotes_and_backslashes() {
        // Alphabet with chars that must be JSON-escaped to be safe to
        // splice into a JS string. Single quote tests the
        // `'-inside-single-quoted-string` path; backslash tests JSON's
        // own escape pass-through.
        let labels = vec!["a".to_string()];
        let s = build_inject_script("a'b\\c", &labels, "div");
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
        let s = build_inject_script("αβγδ", &labels, "div");
        // serde_json escapes non-ASCII into \uXXXX by default; verify
        // the output is valid ASCII so it can't break the surrounding
        // JS string literal regardless of its quote style.
        assert!(s.is_ascii(), "non-ASCII in injected JS:\n{s}");
    }
}
