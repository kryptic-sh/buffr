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

use std::{path::Path, sync::Arc};

use hjkl_bonsai::{
    highlighter::Highlighter,
    runtime::{Grammar, GrammarLoader, GrammarRegistry},
};
use tracing::warn;

/// Maximum source size (10 MiB) before falling back to a size-cap notice.
const MAX_SOURCE_BYTES: usize = 10 * 1024 * 1024;

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

    let text = String::from_utf8_lossy(source);

    // Try syntax-highlighted path.
    match try_highlight(url, source) {
        Some(highlighted) => build_page(url, &highlighted),
        None => {
            let escaped = html_escape(&text);
            build_page(url, &format!("<pre><code>{escaped}</code></pre>"))
        }
    }
}

/// Attempts to syntax-highlight `source` for the language detected from `url`.
///
/// Returns `Some(html_fragment)` on success, `None` when no grammar matches or
/// any step fails (caller falls back to plain `<pre>`).
fn try_highlight(url: &str, source: &[u8]) -> Option<String> {
    let registry = match GrammarRegistry::embedded() {
        Ok(r) => r,
        Err(e) => {
            warn!("buffr-view-source: failed to load grammar registry: {e:#}");
            return None;
        }
    };

    // `name_for_path` and `detect_for_path` take &Path; we pass the URL string
    // as a path — tree-sitter extension detection only looks at the extension
    // component, so this works for ordinary file extensions in URLs.
    let url_path = Path::new(url);

    let lang_name = registry.name_for_path(url_path)?;
    let spec = registry.by_name(lang_name)?;
    let meta = registry.meta();

    let loader = match GrammarLoader::user_default(meta) {
        Ok(l) => l,
        Err(e) => {
            warn!("buffr-view-source: failed to build grammar loader: {e:#}");
            return None;
        }
    };

    let grammar = match Grammar::load(lang_name, spec, &loader, meta) {
        Ok(g) => Arc::new(g),
        Err(e) => {
            warn!("buffr-view-source: failed to load grammar '{lang_name}': {e:#}");
            return None;
        }
    };

    let mut highlighter = match Highlighter::new(grammar) {
        Ok(h) => h,
        Err(e) => {
            warn!("buffr-view-source: failed to create highlighter for '{lang_name}': {e:#}");
            return None;
        }
    };

    let spans = highlighter.highlight(source);
    let text = String::from_utf8_lossy(source);

    // Build highlighted HTML: walk spans in order, emitting spans and
    // unstyled text between them.
    let mut html = String::with_capacity(source.len() * 2);
    html.push_str("<pre><code>");

    let mut cursor = 0usize;

    for span in &spans {
        let range = &span.byte_range;

        // Emit any plain text before this span.
        if cursor < range.start {
            let plain = html_escape(&text[cursor..range.start]);
            html.push_str(&plain);
        }

        // Emit this highlighted span.
        let class = capture_to_class(span.capture());
        let content = html_escape(&text[range.start..range.end]);
        html.push_str(&format!("<span class=\"{class}\">{content}</span>"));

        cursor = range.end;
    }

    // Emit any trailing plain text.
    if cursor < text.len() {
        let tail = html_escape(&text[cursor..]);
        html.push_str(&tail);
    }

    html.push_str("</code></pre>");

    Some(html)
}

/// Converts a capture name like `function.macro` to a CSS class `hl-function-macro`.
fn capture_to_class(capture: &str) -> String {
    let normalized = capture.replace('.', "-");
    format!("hl-{normalized}")
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
    fn capture_to_class_dots_become_dashes() {
        assert_eq!(capture_to_class("function.macro"), "hl-function-macro");
        assert_eq!(capture_to_class("keyword"), "hl-keyword");
    }
}
