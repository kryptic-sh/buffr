//! HTML-escape helpers shared across the browser-process crates.

/// Escape `&`, `<`, `>`, `"` and `'` for safe HTML embedding.
///
/// Safe for both element-text and quoted-attribute contexts. Do **not**
/// use for unquoted attribute values, `<script>`/`<style>` bodies, or URL
/// contexts — those need different encodings entirely.
///
/// Centralised here so the escaping logic is audited once instead of once
/// per crate: a copy that forgets `'` is a latent XSS in whatever renders
/// page-controlled text through it.
pub fn escape(s: &str) -> String {
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
    fn escapes_the_five_dangerous_chars() {
        assert_eq!(escape("<>&\"'"), "&lt;&gt;&amp;&quot;&#39;");
    }

    #[test]
    fn leaves_safe_text_alone() {
        assert_eq!(escape("hello world"), "hello world");
        assert_eq!(escape(""), "");
        assert_eq!(escape("こんにちは"), "こんにちは");
        assert_eq!(escape("café & <bar>"), "café &amp; &lt;bar&gt;");
    }

    #[test]
    fn quoted_attribute_value_is_closed() {
        // A `"` inside the value must not terminate the attribute early.
        let escaped = escape(r#"" onerror="alert(1)"#);
        assert_eq!(escaped, "&quot; onerror=&quot;alert(1)");
    }
}
