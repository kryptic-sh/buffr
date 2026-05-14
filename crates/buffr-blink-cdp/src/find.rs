//! Find-in-page support for the blink-cdp backend (Phase 8b, #83).
//!
//! # Approach
//!
//! CDP has no native find-in-page API. A small JS shim is injected via
//! `Page.addScriptToEvaluateOnNewDocument` that provides three functions
//! in the page's global scope:
//!
//! - `__buffrFindNext(query, caseSensitive)` — highlight all matches via
//!   a TreeWalker scan, scroll the next one into view, return
//!   `{ current: <1-based>, total: <count> }`.
//! - `__buffrFindPrev(query, caseSensitive)` — same but backwards.
//! - `__buffrFindStop()` — remove all highlight spans and clear state.
//!
//! When `start_find` is called, the engine evaluates `__buffrFindNext`
//! (or `__buffrFindPrev` for `forward = false`) via `Runtime.evaluate`,
//! parses the JSON result, and writes the resulting [`FindResult`] into
//! the shared [`FindResultSink`].
//!
//! # Design constraints
//!
//! - No `eval`, no `innerHTML`, no `document.write`.
//! - Span wrapping uses DOM mutation only: `splitText` + `insertBefore` +
//!   `surroundContents`. Text nodes whose content changes under an active
//!   search are rebuilt on the next `start_find` call.
//! - The shim is idempotent: calling `__buffrFindNext` with a new query
//!   on an already-highlighted page first calls `__buffrFindStop`.
//! - Highlight colour matches Firefox's yellow default (`#ffff00`, `#ff9632`
//!   for current match) so pages don't need special treatment.
//!
//! # CDP round-trip latency
//!
//! `Runtime.evaluate` on a local headless process completes in ~1 ms.
//! Calling `start_find` on every keystroke (after debounce) is safe.

use buffr_core::find::FindResult;
use serde_json::Value;

// ── JS shim ───────────────────────────────────────────────────────────────────

/// Generate the JS shim source injected via
/// `Page.addScriptToEvaluateOnNewDocument`.
///
/// Provides three globals on the page:
///   - `__buffrFindNext(query, caseSensitive)` → `{ current, total }` JSON
///   - `__buffrFindPrev(query, caseSensitive)` → `{ current, total }` JSON
///   - `__buffrFindStop()` → `undefined`
///
/// All DOM mutation is performed via standard DOM APIs (TreeWalker,
/// `splitText`, `insertBefore`, `parentNode.removeChild`). No `eval`,
/// no `innerHTML`, no `document.write`.
pub fn find_shim_js() -> &'static str {
    r#"
(function () {
  'use strict';

  // ── State ────────────────────────────────────────────────────────────────────

  var _state = {
    query: '',
    caseSensitive: false,
    matches: [],   // Array of <span class="__buffr-find-match"> elements
    current: -1    // index into matches; -1 = none active
  };

  // ── CSS injection ────────────────────────────────────────────────────────────

  function _ensureStyles() {
    if (document.getElementById('__buffr-find-styles')) return;
    var s = document.createElement('style');
    s.id = '__buffr-find-styles';
    s.textContent =
      '.__buffr-find-match { background: #ffff00 !important; color: #000 !important; border-radius: 2px; }' +
      '.__buffr-find-current { background: #ff9632 !important; color: #000 !important; border-radius: 2px; outline: 2px solid #f0590a !important; }';
    (document.head || document.documentElement).appendChild(s);
  }

  // ── Clear ────────────────────────────────────────────────────────────────────

  function _clearMatches() {
    // Replace each highlight span with its text-node child, then
    // normalise the parent so adjacent text nodes merge.
    var spans = document.querySelectorAll('.__buffr-find-match');
    var parents = [];
    for (var i = 0; i < spans.length; i++) {
      var sp = spans[i];
      var parent = sp.parentNode;
      if (!parent) continue;
      // Move children out of the span, then remove it.
      while (sp.firstChild) {
        parent.insertBefore(sp.firstChild, sp);
      }
      parent.removeChild(sp);
      if (parents.indexOf(parent) === -1) parents.push(parent);
    }
    // Normalise text nodes so future scans work cleanly.
    for (var j = 0; j < parents.length; j++) {
      if (parents[j].normalize) parents[j].normalize();
    }
    _state.matches = [];
    _state.current = -1;
  }

  // ── Scan and highlight ────────────────────────────────────────────────────────

  function _scan(query, caseSensitive) {
    _ensureStyles();
    _clearMatches();
    if (!query) return;

    var searchQuery = caseSensitive ? query : query.toLowerCase();
    var walker = document.createTreeWalker(
      document.body,
      0x4, // NodeFilter.SHOW_TEXT
      null
    );

    var node;
    var matchSpans = [];

    while ((node = walker.nextNode())) {
      var text = caseSensitive ? node.nodeValue : (node.nodeValue || '').toLowerCase();
      var idx;
      var offset = 0;
      // Find all non-overlapping occurrences in this text node.
      while ((idx = text.indexOf(searchQuery, offset)) !== -1) {
        // Split off the part before the match, if any.
        var matchNode = node;
        if (idx > 0) {
          matchNode = node.splitText(idx);
          // Advance the walker past the new prefix node.
          // (walker.nextNode would visit the split remainder next, but we
          // are operating on matchNode directly so we only need to update
          // text for the next indexOf pass.)
          text = caseSensitive
            ? matchNode.nodeValue
            : (matchNode.nodeValue || '').toLowerCase();
          offset = 0;
          idx = 0;
        }
        // Split off the match itself from the rest.
        var afterMatch;
        if (matchNode.nodeValue.length > query.length) {
          afterMatch = matchNode.splitText(query.length);
        }

        // Wrap the match node in a highlight span.
        var span = document.createElement('span');
        span.className = '__buffr-find-match';
        matchNode.parentNode.insertBefore(span, matchNode);
        span.appendChild(matchNode);
        matchSpans.push(span);

        if (afterMatch) {
          // Continue scanning in the remainder node.
          node = afterMatch;
          text = caseSensitive ? node.nodeValue : (node.nodeValue || '').toLowerCase();
          offset = 0;
        } else {
          break;
        }
      }
    }

    _state.query = query;
    _state.caseSensitive = caseSensitive;
    _state.matches = matchSpans;
    _state.current = -1;
  }

  // ── Navigate ─────────────────────────────────────────────────────────────────

  function _activate(idx) {
    if (_state.matches.length === 0) return;
    // Remove current highlight from previous.
    if (_state.current >= 0 && _state.current < _state.matches.length) {
      _state.matches[_state.current].classList.remove('__buffr-find-current');
    }
    // Wrap-around.
    _state.current = ((idx % _state.matches.length) + _state.matches.length) % _state.matches.length;
    var el = _state.matches[_state.current];
    el.classList.add('__buffr-find-current');
    if (el.scrollIntoView) {
      el.scrollIntoView({ block: 'center', inline: 'nearest', behavior: 'instant' });
    }
  }

  // ── Public API ────────────────────────────────────────────────────────────────

  window.__buffrFindNext = function (query, caseSensitive) {
    if (!query) {
      _clearMatches();
      return JSON.stringify({ current: 0, total: 0 });
    }
    // Re-scan if query or case changed.
    if (query !== _state.query || caseSensitive !== _state.caseSensitive) {
      _scan(query, !!caseSensitive);
    }
    if (_state.matches.length === 0) {
      return JSON.stringify({ current: 0, total: 0 });
    }
    _activate(_state.current + 1);
    return JSON.stringify({ current: _state.current + 1, total: _state.matches.length });
  };

  window.__buffrFindPrev = function (query, caseSensitive) {
    if (!query) {
      _clearMatches();
      return JSON.stringify({ current: 0, total: 0 });
    }
    if (query !== _state.query || caseSensitive !== _state.caseSensitive) {
      _scan(query, !!caseSensitive);
    }
    if (_state.matches.length === 0) {
      return JSON.stringify({ current: 0, total: 0 });
    }
    var prevIdx = _state.current <= 0 ? _state.matches.length - 1 : _state.current - 1;
    _activate(prevIdx);
    return JSON.stringify({ current: _state.current + 1, total: _state.matches.length });
  };

  window.__buffrFindStop = function () {
    _clearMatches();
    _state.query = '';
  };
})();
"#
}

// ── Result parsing ────────────────────────────────────────────────────────────

/// Build the JS expression to call `__buffrFindNext` or `__buffrFindPrev`
/// with a safely escaped query string.
///
/// Returns a JS expression whose evaluation yields a JSON string
/// `{ current: <1-based>, total: <count> }`.
///
/// The query is escaped by JSON-encoding it via `serde_json`, so it is
/// safe to embed in a JS string literal without manual escaping.
pub fn find_expr(query: &str, case_sensitive: bool, forward: bool) -> String {
    // serde_json::to_string always produces valid JSON; unwrap is safe.
    let js_str = serde_json::to_string(query).unwrap_or_else(|_| "\"\"".to_owned());
    let func = if forward {
        "__buffrFindNext"
    } else {
        "__buffrFindPrev"
    };
    let cs = if case_sensitive { "true" } else { "false" };
    format!("{func}({js_str}, {cs})")
}

/// Build the JS expression to call `__buffrFindStop`.
pub fn stop_expr() -> &'static str {
    "__buffrFindStop()"
}

/// Parse a `Runtime.evaluate` response value into a [`FindResult`].
///
/// The JS functions return a JSON string `{ current: <u32>, total: <u32> }`.
/// `Runtime.evaluate` wraps the returned value in `result.value` (a JSON
/// string when the JS return type is a primitive string).
///
/// Returns `None` on parse failure — the caller logs and no-ops.
pub fn parse_find_result(value: &Value) -> Option<FindResult> {
    // `result.value` is a JSON-encoded string when the expression returns a string.
    let json_str = value
        .get("result")
        .and_then(|r| r.get("value"))
        .and_then(|v| v.as_str())?;

    let parsed: Value = serde_json::from_str(json_str).ok()?;
    let current = parsed.get("current")?.as_u64()? as u32;
    let total = parsed.get("total")?.as_u64()? as u32;

    Some(FindResult {
        count: total,
        current,
        // JS scan is synchronous — every result is final.
        final_update: true,
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn find_expr_forward_case_insensitive() {
        let expr = find_expr("hello world", false, true);
        assert!(expr.starts_with("__buffrFindNext("));
        assert!(expr.contains("\"hello world\""));
        assert!(expr.contains("false"));
    }

    #[test]
    fn find_expr_backward_case_sensitive() {
        let expr = find_expr("Rust", true, false);
        assert!(expr.starts_with("__buffrFindPrev("));
        assert!(expr.contains("\"Rust\""));
        assert!(expr.contains("true"));
    }

    #[test]
    fn find_expr_escapes_special_chars() {
        // serde_json escapes backslash, quotes, newlines.
        let expr = find_expr("foo\nbar\"baz", false, true);
        assert!(!expr.contains('\n'));
        assert!(expr.contains("\\n"));
    }

    #[test]
    fn parse_find_result_valid() {
        let value = json!({
            "result": {
                "type": "string",
                "value": "{\"current\":3,\"total\":7}"
            }
        });
        let r = parse_find_result(&value).expect("should parse");
        assert_eq!(r.current, 3);
        assert_eq!(r.count, 7);
        assert!(r.final_update);
    }

    #[test]
    fn parse_find_result_zero() {
        let value = json!({
            "result": {
                "type": "string",
                "value": "{\"current\":0,\"total\":0}"
            }
        });
        let r = parse_find_result(&value).expect("should parse zero");
        assert_eq!(r.current, 0);
        assert_eq!(r.count, 0);
    }

    #[test]
    fn parse_find_result_missing_result_key() {
        let value = json!({ "foo": "bar" });
        assert!(parse_find_result(&value).is_none());
    }

    #[test]
    fn parse_find_result_malformed_json_string() {
        let value = json!({
            "result": { "type": "string", "value": "not-json" }
        });
        assert!(parse_find_result(&value).is_none());
    }

    #[test]
    fn find_shim_js_contains_key_functions() {
        let shim = find_shim_js();
        assert!(shim.contains("__buffrFindNext"));
        assert!(shim.contains("__buffrFindPrev"));
        assert!(shim.contains("__buffrFindStop"));
        assert!(!shim.contains("eval("));
        assert!(!shim.contains("innerHTML"));
        assert!(!shim.contains("document.write"));
    }

    #[test]
    fn stop_expr_is_correct() {
        assert_eq!(stop_expr(), "__buffrFindStop()");
    }
}
