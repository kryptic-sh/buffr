//! Shared HTML escaping.
//!
//! There used to be two `html_escape` functions in this crate with
//! *different* escape sets — `new_tab.rs` escaped only `& < >` while
//! `view_source_scheme.rs` escaped `& < > " '` (L13). Two same-named
//! helpers with different security properties invite the weaker one being
//! reused in an attribute context, so only the strict version survives and
//! it lives here.

/// HTML-escape `&`, `<`, `>`, `"` and `'`.
///
/// Safe for both element-text and quoted-attribute contexts. Do **not**
/// use for unquoted attribute values, `<script>`/`<style>` bodies, or URL
/// contexts — those need different encodings entirely.
pub(crate) fn html_escape(s: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_all_five_metacharacters() {
        assert_eq!(html_escape("<>&\"'"), "&lt;&gt;&amp;&quot;&#39;");
    }

    #[test]
    fn leaves_plain_text_untouched() {
        assert_eq!(html_escape("hello world"), "hello world");
        assert_eq!(html_escape(""), "");
    }

    #[test]
    fn escapes_attribute_breakout_attempt() {
        // The reason the weak `& < >`-only variant was deleted: it left
        // this payload able to escape a quoted attribute.
        let escaped = html_escape(r#"" onerror="alert(1)"#);
        assert!(!escaped.contains('"'));
        assert!(escaped.contains("&quot;"));
    }

    #[test]
    fn preserves_non_ascii() {
        assert_eq!(html_escape("こんにちは"), "こんにちは");
        assert_eq!(html_escape("café & <bar>"), "café &amp; &lt;bar&gt;");
    }
}
