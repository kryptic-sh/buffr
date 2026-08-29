//! Shared HTML escaping.
//!
//! There used to be two `html_escape` functions in this crate with
//! *different* escape sets — `new_tab.rs` escaped only `& < >` while
//! `view_source_scheme.rs` escaped `& < > " '` (L13). Two same-named
//! helpers with different security properties invite the weaker one being
//! reused in an attribute context, so only the strict version survives.
//! It now lives in `buffr_core::html` and is re-exported here so this
//! crate's call sites keep a one-line path.

/// HTML-escape `&`, `<`, `>`, `"` and `'`.
///
/// Safe for both element-text and quoted-attribute contexts. Do **not**
/// use for unquoted attribute values, `<script>`/`<style>` bodies, or URL
/// contexts — those need different encodings entirely.
pub(crate) use buffr_core::html::escape as html_escape;

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
