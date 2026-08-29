//! View-source renderer for buffr.
//!
//! Converts raw page source bytes into a self-contained, syntax-highlighted
//! HTML page suitable for display in a `buffr-src:` tab. The output uses only
//! inline CSS — no external assets — so it can be served as a `data:text/html`
//! URL or via a custom CEF scheme handler without any additional resource
//! loading.
//!
//! # Example
//!
//! ```rust
//! let html = buffr_view_source::render("https://example.com/main.rs", b"fn main() {}");
//! assert!(html.starts_with("<!DOCTYPE html>"));
//! ```

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use hjkl_bonsai::{
    highlighter::Highlighter,
    runtime::{Grammar, GrammarLoader, GrammarRegistry},
};
use tracing::warn;

/// Maximum source size (10 MiB) before falling back to a size-cap notice.
const MAX_SOURCE_BYTES: usize = 10 * 1024 * 1024;

/// Registry + loader pair, built once per process. Rebuilding either on
/// every view-source request re-parsed the embedded `bonsai.toml`
/// manifest and re-resolved the XDG data/cache dirs each time (perf
/// §19-1); the `&str` returned by `name_for_path` borrows from the
/// registry, so once this lives in a `OnceLock` the language names are
/// `'static` and can key the grammar cache below.
struct GrammarEnv {
    registry: GrammarRegistry,
    loader: GrammarLoader,
}

static GRAMMAR_ENV: OnceLock<Option<GrammarEnv>> = OnceLock::new();

/// Loaded grammars by canonical language name. The `dlopen` + query
/// parse happens once per language per process; later requests for the
/// same language hit the map (hjkl-bonsai additionally caches the
/// compiled query artifacts process-globally, keyed by content hash).
/// `None` is cached when the environment itself cannot be built.
static GRAMMARS: OnceLock<Mutex<HashMap<&'static str, Arc<Grammar>>>> = OnceLock::new();

/// Languages whose grammar failed to load. A load failure is not
/// transient — the artifact either exists on disk or it does not — so a
/// failed dlopen is cached too, or every view-source request for that
/// language re-runs the load + query parse before falling back to `<pre>`
/// (perf §22 D-P2).
static FAILED_GRAMMARS: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();

fn grammar_env() -> Option<&'static GrammarEnv> {
    GRAMMAR_ENV
        .get_or_init(|| {
            let registry = GrammarRegistry::embedded().ok()?;
            let loader = GrammarLoader::user_default(registry.meta()).ok()?;
            Some(GrammarEnv { registry, loader })
        })
        .as_ref()
}

/// Renders a syntax-highlighted HTML page for `view-source:<url>`.
///
/// `url` is the underlying URL (used for language detection via extension and
/// shown in the page title); `source` is the raw response body.
///
/// Returns a self-contained HTML string with inline CSS — no external assets —
/// so the result can be served via a `data:text/html;...` URL or a custom CEF
/// response filter.
///
/// Falls back to plain `<pre>` when:
/// - No grammar matches the URL's extension.
/// - Grammar resolution or loading fails.
/// - Source exceeds 10 MiB (a "source too large" notice is shown instead).
///
/// Never panics on arbitrary input; source bytes are decoded as UTF-8 lossy.
pub fn render(url: &str, source: &[u8]) -> String {
    // Size cap: avoid OOM on enormous bundles.
    if source.len() > MAX_SOURCE_BYTES {
        let notice = html_escape("(source too large to highlight — reduce to under 10 MiB)");
        return build_page(url, &format!("<pre>{notice}</pre>"));
    }

    // Decode lossily exactly once, up front, and highlight the *decoded*
    // bytes. Highlighter span offsets index whatever slice they were
    // produced from; if we highlighted the raw bytes but sliced the decoded
    // string, every invalid byte (which becomes a 3-byte U+FFFD) would shift
    // the two out of sync and the slices would land mid-codepoint.
    let text = String::from_utf8_lossy(source);

    // Try syntax-highlighted path.
    match try_highlight(url, &text) {
        Some(highlighted) => build_page(url, &highlighted),
        None => {
            let escaped = html_escape(&text);
            build_page(url, &format!("<pre><code>{escaped}</code></pre>"))
        }
    }
}

/// Attempts to syntax-highlight `text` for the language detected from `url`.
///
/// `text` must already be valid UTF-8 (the caller lossy-decodes once); span
/// offsets are taken against `text.as_bytes()` so they always agree with the
/// string we slice.
///
/// Returns `Some(html_fragment)` on success, `None` when no grammar matches or
/// any step fails (caller falls back to plain `<pre>`).
fn try_highlight(url: &str, text: &str) -> Option<String> {
    let env = grammar_env()?;

    // `name_for_path` and `detect_for_path` take &Path; we pass the URL string
    // as a path — tree-sitter extension detection only looks at the extension
    // component, so this works for ordinary file extensions in URLs.
    let url_path = Path::new(url);

    let lang_name = env.registry.name_for_path(url_path)?;

    // A6: never clone/compile a grammar over the network from the render
    // path — hjkl-bonsai's Grammar::load does exactly that on a cache miss
    // (git clone + system C/C++ compiler + dlopen, in-process). Resolve only
    // installed artifacts and fall back to plain <pre> when none exists.
    let Some(so) = env.loader.lookup_only(lang_name) else {
        tracing::debug!(
            "buffr-view-source: no installed grammar artifact for '{lang_name}'; skipping highlight"
        );
        return None;
    };

    // Grammar cache (perf §19-1): the dlopen + query parse runs once per
    // language per process. `lang_name` is `'static` (borrowed from the
    // static registry), so it keys the map directly. Load *failures* are
    // cached too (perf §22 D-P2): a failed dlopen is not transient, so
    // re-running it per request just repeats the work before the same
    // `<pre>` fallback.
    let grammar = {
        let failed = FAILED_GRAMMARS.get_or_init(|| Mutex::new(HashSet::new()));
        if failed.lock().ok()?.contains(lang_name) {
            return None;
        }
        let cache = GRAMMARS.get_or_init(|| Mutex::new(HashMap::new()));
        let mut cache = cache.lock().ok()?;
        if let Some(g) = cache.get(lang_name) {
            Arc::clone(g)
        } else {
            let g = match Grammar::load_from_path(lang_name, &so) {
                Ok(g) => Arc::new(g),
                Err(e) => {
                    warn!("buffr-view-source: failed to load grammar '{lang_name}': {e:#}");
                    if let Ok(mut failed) = failed.lock() {
                        failed.insert(lang_name);
                    }
                    return None;
                }
            };
            cache.insert(lang_name, Arc::clone(&g));
            g
        }
    };

    let mut highlighter = match Highlighter::new(grammar) {
        Ok(h) => h,
        Err(e) => {
            warn!("buffr-view-source: failed to create highlighter for '{lang_name}': {e:#}");
            return None;
        }
    };

    let spans = highlighter.highlight(text.as_bytes());

    Some(render_spans(
        text,
        spans
            .iter()
            .map(|span| (span.byte_range.clone(), span.capture())),
    ))
}

/// Walks `spans` in order, emitting `<span>`s for highlighted ranges and
/// escaped plain text in between.
///
/// Split out from [`try_highlight`] so the slicing can be tested without a
/// grammar: loading one depends on the embedded registry, which is not
/// available in every build, and this loop is where H3 lived.
///
/// `spans` carry byte offsets into `text`, and every slice goes through
/// `str::get`, so a span that is out of order, inverted, past the end, or
/// not on a char boundary is skipped rather than panicking. That matters
/// because `text` is lossy-decoded page source: nothing upstream guarantees
/// the grammar's offsets line up with it.
fn render_spans<'a>(
    text: &str,
    spans: impl IntoIterator<Item = (std::ops::Range<usize>, &'a str)>,
) -> String {
    let mut html = String::with_capacity(text.len() * 2);
    html.push_str("<pre><code>");

    let mut cursor = 0usize;

    for (range, capture) in spans {
        if range.start < cursor || range.end < range.start {
            continue;
        }
        let (Some(plain), Some(content)) = (
            text.get(cursor..range.start),
            text.get(range.start..range.end),
        ) else {
            continue;
        };

        // Emit any plain text before this span.
        push_escaped(&mut html, plain);

        // Emit this highlighted span. The palette styles a bounded set of
        // capture names (the CSS block enumerates exactly these classes);
        // anything outside it falls back to the generic dotted form so an
        // unstyled capture still gets a span.
        html.push_str("<span class=\"");
        match capture_to_class(capture) {
            Some(class) => html.push_str(class),
            None => {
                let class = format!("hl-{}", capture.replace('.', "-"));
                html.push_str(&class);
            }
        }
        html.push_str("\">");
        push_escaped(&mut html, content);
        html.push_str("</span>");

        cursor = range.end;
    }

    // Emit any trailing plain text. `cursor` is always a char boundary
    // (it only ever advances to the end of a successfully sliced range).
    if let Some(tail) = text.get(cursor..) {
        push_escaped(&mut html, tail);
    }

    html.push_str("</code></pre>");

    html
}

/// Maps a capture name like `function.macro` to its CSS class
/// `hl-function-macro`. Returns `None` for names outside the palette's
/// bounded set — the caller falls back to the generic dotted form. The
/// `&'static str` result means the common path (grammars emit exactly
/// these names) allocates nothing per span, whereas the old
/// `replace` + `format!` allocated twice per span (perf §19-2).
fn capture_to_class(capture: &str) -> Option<&'static str> {
    Some(match capture {
        "keyword" => "hl-keyword",
        "keyword.control" => "hl-keyword-control",
        "keyword.operator" => "hl-keyword-operator",
        "function" => "hl-function",
        "function.macro" => "hl-function-macro",
        "function.method" => "hl-function-method",
        "string" => "hl-string",
        "string.special" => "hl-string-special",
        "comment" => "hl-comment",
        "comment.line" => "hl-comment-line",
        "comment.block" => "hl-comment-block",
        "number" => "hl-number",
        "type" => "hl-type",
        "type.builtin" => "hl-type-builtin",
        "variable" => "hl-variable",
        "variable.builtin" => "hl-variable-builtin",
        "constant" => "hl-constant",
        "constant.builtin" => "hl-constant-builtin",
        "operator" => "hl-operator",
        "punctuation" => "hl-punctuation",
        "punctuation.bracket" => "hl-punctuation-bracket",
        "punctuation.delimiter" => "hl-punctuation-delimiter",
        "attribute" => "hl-attribute",
        "label" => "hl-label",
        "namespace" => "hl-namespace",
        "module" => "hl-module",
        _ => return None,
    })
}

/// Appends `s` to `out`, escaping HTML metacharacters as it goes. The
/// per-span escape previously allocated a fresh `String` and the
/// `<span>` `format!` copied the escaped bytes again (perf §19-2).
fn push_escaped(out: &mut String, s: &str) {
    for ch in s.chars() {
        match ch {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
}

/// Escapes `<`, `>`, `&`, `"`, and `'` for safe HTML embedding.
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// Wraps `body_fragment` in a minimal self-contained HTML5 document.
fn build_page(url: &str, body_fragment: &str) -> String {
    let title = html_escape(&format!("view-source: {url}"));
    format!(
        r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<title>{title}</title>
<style>
/* buffr view-source — placeholder palette (R3 ports Tokyonight) */
:root {{
  --bg: #1a1b26;
  --fg: #c0caf5;
  --hl-keyword: #bb9af7;
  --hl-function: #7aa2f7;
  --hl-string: #9ece6a;
  --hl-comment: #565f89;
  --hl-number: #ff9e64;
  --hl-type: #2ac3de;
  --hl-variable: #c0caf5;
  --hl-constant: #ff9e64;
  --hl-operator: #89ddff;
  --hl-punctuation: #89ddff;
}}
@media (prefers-color-scheme: light) {{
  :root {{
    --bg: #f5f5f5;
    --fg: #343b58;
    --hl-keyword: #9854f1;
    --hl-function: #2e7de9;
    --hl-string: #587539;
    --hl-comment: #848cb5;
    --hl-number: #b15c00;
    --hl-type: #007197;
    --hl-variable: #343b58;
    --hl-constant: #b15c00;
    --hl-operator: #006a83;
    --hl-punctuation: #006a83;
  }}
}}
html, body {{
  margin: 0;
  padding: 0;
  background: var(--bg);
  color: var(--fg);
}}
pre, code {{
  font-family: "SF Mono", Menlo, Consolas, monospace;
  font-size: 13px;
  line-height: 1.5;
  tab-size: 4;
}}
pre {{
  margin: 0;
  padding: 1em 1.5em;
  overflow: auto;
}}
/* Common capture classes */
.hl-keyword              {{ color: var(--hl-keyword); font-weight: bold; }}
.hl-keyword-control      {{ color: var(--hl-keyword); font-weight: bold; }}
.hl-keyword-operator     {{ color: var(--hl-operator); }}
.hl-function             {{ color: var(--hl-function); }}
.hl-function-macro       {{ color: var(--hl-function); }}
.hl-function-method      {{ color: var(--hl-function); }}
.hl-string               {{ color: var(--hl-string); }}
.hl-string-special       {{ color: var(--hl-string); }}
.hl-comment              {{ color: var(--hl-comment); font-style: italic; }}
.hl-comment-line         {{ color: var(--hl-comment); font-style: italic; }}
.hl-comment-block        {{ color: var(--hl-comment); font-style: italic; }}
.hl-number               {{ color: var(--hl-number); }}
.hl-type                 {{ color: var(--hl-type); }}
.hl-type-builtin         {{ color: var(--hl-type); }}
.hl-variable             {{ color: var(--hl-variable); }}
.hl-variable-builtin     {{ color: var(--hl-variable); font-style: italic; }}
.hl-constant             {{ color: var(--hl-constant); }}
.hl-constant-builtin     {{ color: var(--hl-constant); font-weight: bold; }}
.hl-operator             {{ color: var(--hl-operator); }}
.hl-punctuation          {{ color: var(--hl-punctuation); }}
.hl-punctuation-bracket  {{ color: var(--hl-punctuation); }}
.hl-punctuation-delimiter{{ color: var(--hl-punctuation); }}
.hl-attribute            {{ color: var(--hl-keyword); }}
.hl-label                {{ color: var(--hl-keyword); }}
.hl-namespace            {{ color: var(--hl-type); }}
.hl-module               {{ color: var(--hl-type); }}
</style>
</head>
<body>
{body_fragment}
</body>
</html>"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_escape_replaces_special_chars() {
        assert_eq!(html_escape("<>&\"'"), "&lt;&gt;&amp;&quot;&#39;");
    }

    #[test]
    fn capture_to_class_maps_palette_names_statically() {
        assert_eq!(
            capture_to_class("function.macro"),
            Some("hl-function-macro")
        );
        assert_eq!(capture_to_class("keyword"), Some("hl-keyword"));
        assert_eq!(
            capture_to_class("punctuation.bracket"),
            Some("hl-punctuation-bracket")
        );
        // Names outside the palette get no static class — the caller
        // falls back to the generic dotted form.
        assert_eq!(capture_to_class("unknown.capture"), None);
    }

    #[test]
    fn render_spans_falls_back_to_generic_class_for_unknown_captures() {
        let text = "let x = 1;";
        let html = render_spans(text, [(0..1, "unknown.capture")]);
        assert!(
            html.contains("<span class=\"hl-unknown-capture\">l</span>"),
            "expected the generic fallback class, got: {html}"
        );
    }

    #[test]
    fn push_escaped_matches_html_escape() {
        let mut out = String::new();
        push_escaped(&mut out, "<>&\"'café");
        assert_eq!(out, html_escape("<>&\"'café"));
    }

    #[test]
    fn render_invalid_utf8_does_not_panic() {
        // Stray 0xFF bytes each decode to a 3-byte U+FFFD, so the lossy
        // string is longer than the raw source. Highlighting the raw bytes
        // and slicing the decoded string used to land mid-replacement-char
        // and panic with "byte index is not a char boundary" (H3).
        let source = b"fn main() {\n    let s = \"\xff\xfe caf\xe9\";\n}\n";
        let html = render("https://example.com/main.rs", source);
        assert!(html.starts_with("<!DOCTYPE html>"));
        // Every invalid byte survives as exactly one U+FFFD — nothing was
        // dropped by the boundary guard and nothing was duplicated.
        assert_eq!(
            html.matches('\u{FFFD}').count(),
            3,
            "expected the three invalid bytes to survive as U+FFFD"
        );
        assert!(html.contains("main"));
        // `let s` is emitted verbatim; nothing before the bad bytes is lost.
        assert!(html.contains("let"));
        // NOTE: deliberately no assertion that highlighting ran. Whether a
        // grammar loads depends on the embedded registry being present,
        // which is not true in every build (it is absent on CI), so
        // requiring `<span class="hl-` here made the test environment
        // dependent. The slicing this test exists to guard is covered
        // directly by the `render_spans_*` tests below, which need no
        // grammar.
    }

    /// H3 lived in the span walk: spans index the raw bytes, the string
    /// being sliced is lossy-decoded, and each invalid byte grows from 1
    /// byte to 3 as U+FFFD — so offsets drift and land mid-character.
    /// These drive that loop directly, without a grammar.
    #[test]
    fn render_spans_skips_offsets_that_are_not_char_boundaries() {
        // "a\u{FFFD}b": the replacement char occupies bytes 1..4, so 2 and
        // 3 are interior. A span landing there must be skipped, not panic.
        let text = "a\u{FFFD}b";
        for (start, end) in [(0, 2), (2, 4), (1, 3), (3, 5)] {
            let html = render_spans(text, [(start..end, "keyword")]);
            assert!(html.starts_with("<pre><code>"), "{start}..{end}");
            assert!(html.ends_with("</code></pre>"), "{start}..{end}");
        }
    }

    #[test]
    fn render_spans_skips_out_of_order_inverted_and_past_the_end() {
        let text = "let x = 1;";
        // Out of order (second starts before the first ended), inverted,
        // and past the end. None may panic; none may lose the text.
        // The inverted range is built from variables — a `9..2` literal is
        // a clippy `reversed_empty_ranges` error.
        let (hi, lo) = (9usize, 2usize);
        let html = render_spans(
            text,
            [(4..7, "variable"), (0..3, "keyword"), (hi..lo, "operator")],
        );
        assert!(html.contains("<span class=\"hl-variable\">x =</span>"));
        assert!(
            !html.contains("hl-keyword"),
            "backwards span must be skipped"
        );
        let html = render_spans(text, [(0..999, "keyword")]);
        assert!(
            html.contains("let x = 1;"),
            "past-the-end span must be skipped"
        );
    }

    #[test]
    fn render_spans_emits_all_text_exactly_once() {
        let text = "fn main() {}";
        let html = render_spans(text, [(0..2, "keyword"), (3..7, "function")]);
        assert_eq!(
            html,
            "<pre><code><span class=\"hl-keyword\">fn</span> \
             <span class=\"hl-function\">main</span>() {}</code></pre>"
        );
    }

    #[test]
    fn render_spans_escapes_inside_and_outside_spans() {
        let text = "<a> & <b>";
        let html = render_spans(text, [(0..3, "tag")]);
        assert!(html.contains("<span class=\"hl-tag\">&lt;a&gt;</span>"));
        assert!(html.contains("&amp;"));
        assert!(!html.contains("<a>"), "raw markup must not survive");
    }

    #[test]
    fn render_invalid_utf8_unknown_extension_does_not_panic() {
        let html = render("https://example.com/blob.unknownext", b"\xff\xfe\xff\xfe");
        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(html.contains('\u{FFFD}'));
    }

    #[test]
    fn render_lone_invalid_byte_is_lossless_in_length() {
        // A single 0xFF at every offset of an otherwise-ASCII buffer.
        for i in 0..12usize {
            let mut source = b"fn a(){b();}".to_vec();
            source[i] = 0xFF;
            let html = render("https://example.com/x.rs", &source);
            assert!(html.starts_with("<!DOCTYPE html>"), "failed at offset {i}");
        }
    }

    #[test]
    fn render_multibyte_utf8_is_preserved() {
        let html = render(
            "https://example.com/main.rs",
            "fn main() { \"café €\"; }".as_bytes(),
        );
        assert!(html.contains("caf\u{e9}"));
        assert!(html.contains('\u{20ac}'));
    }

    #[test]
    fn render_empty_source() {
        let html = render("https://example.com/main.rs", b"");
        assert!(html.starts_with("<!DOCTYPE html>"));
    }
}
