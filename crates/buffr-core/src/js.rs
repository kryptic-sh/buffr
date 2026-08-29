//! JS string-literal escaping shared across the browser-process crates.

/// Escape `s` for safe splicing into a JS string literal, forcing every
/// non-ASCII codepoint to `\uXXXX` so the result is pure ASCII.
///
/// Escapes **both** quote characters (`'` and `"`). Escaping a quote
/// that is not the literal's delimiter is harmless — `\"` inside a
/// `'...'` literal and `\'` inside a `"..."` literal both evaluate to
/// the bare quote — so this one body is safe for single- and
/// double-quoted contexts alike. The caller supplies the surrounding
/// quotes.
///
/// The `\uXXXX` forcing is deliberate: the injected JS travels through
/// CEF's `execute_java_script` UTF-8 path uninspected, and non-ASCII
/// bytes there defeat the "ASCII-only, regardless of input" guarantee
/// the hint/edit payloads promise.
///
/// Centralised here so the escaping logic is audited once instead of
/// once per crate (A-T1); the three copies it replaces differed only in
/// which quote they escaped and drifted silently.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\'' => out.push_str("\\'"),
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // Plain printable ASCII passes through.
            c if c.is_ascii_graphic() || c == ' ' => out.push(c),
            // Everything else (control chars, non-ASCII): emit \uXXXX
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

    #[test]
    fn escapes_both_quotes_and_backslash() {
        assert_eq!(escape("'"), "\\'");
        assert_eq!(escape("\""), "\\\"");
        assert_eq!(escape("\\"), "\\\\");
        // Both quotes escaped regardless of context.
        assert_eq!(escape("'\"'"), "\\'\\\"\\'");
    }

    #[test]
    fn escapes_control_chars() {
        assert_eq!(escape("\n"), "\\n");
        assert_eq!(escape("\r"), "\\r");
        assert_eq!(escape("\t"), "\\t");
    }

    #[test]
    fn forces_non_ascii_to_ascii_escapes() {
        // BMP: \uXXXX. Supplementary plane: a surrogate pair.
        assert_eq!(escape("é"), "\\u00e9");
        assert_eq!(escape("漢"), "\\u6f22");
        // U+1F600 😀 → surrogate pair D83D DE00.
        assert_eq!(escape("😀"), "\\ud83d\\ude00");
        // Result is pure ASCII even for a fully non-ASCII input.
        assert!(escape("é漢😀").is_ascii());
    }

    #[test]
    fn passes_through_ascii_and_space() {
        assert_eq!(escape("hello world"), "hello world");
        assert_eq!(escape(""), "");
    }

    #[test]
    fn single_quoted_context_still_valid() {
        // Escaping the double quote inside a single-quoted literal is
        // harmless — JS evaluates it to the bare quote.
        let lit = format!("'{}'", escape("a\"b"));
        assert_eq!(lit, "'a\\\"b'");
    }

    #[test]
    fn double_quoted_context_still_valid() {
        // And vice versa.
        let lit = format!("\"{}\"", escape("a'b"));
        assert_eq!(lit, "\"a\\'b\"");
    }
}
