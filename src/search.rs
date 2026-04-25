//! URL-or-search resolution for the omnibar.
//!
//! Phase 3 chrome: when the user types into the omnibar and presses
//! Enter, we need to decide whether the input is a navigable URL or a
//! free-form search query. The rule:
//!
//! 1. Parses as a `url::Url` with a scheme (e.g. `https://example.com`,
//!    `file:///etc/passwd`) → use as-is.
//! 2. Looks like `host(:port)?(/path)?` (a scheme-less URL — `example.com`,
//!    `localhost:3000`, `192.168.1.1/foo`) → prepend `https://` for
//!    bare hosts and `http://` for `localhost`/IPv4.
//! 3. Anything else → search-engine fallback. The query is URL-encoded
//!    and substituted into the configured engine's `{query}` template.
//!
//! Edge cases:
//!
//! - Empty / whitespace-only input: returns an empty string. Caller is
//!   expected to short-circuit before calling.
//! - Input contains internal whitespace ("foo bar"): always a search
//!   query (URLs don't have whitespace).
//! - Single-word inputs without a dot ("foobar"): always a search
//!   query — bare hostnames need a TLD or be `localhost`.

use crate::Search;

/// Resolve a raw input string to a navigable URL.
///
/// See module docs for the resolution rules. The returned string is
/// always a fully-qualified URL — never a bare host, never empty for
/// non-empty input.
///
/// `search` provides the engine table; the `default_engine` lookup
/// falls back to `"https://duckduckgo.com/?q={query}"` if the
/// configured engine isn't present (defensive — `validate` already
/// enforces the engine exists, but this function should never panic
/// even on a malformed config).
pub fn resolve_input(input: &str, search: &Search) -> String {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    // Branch 1: parses as a full URL with a recognised scheme.
    //
    // `url::Url::parse` is permissive — it happily reads
    // `localhost:3000` as scheme=`localhost`, path=`3000`. We only
    // treat the parse as a "real URL" when the scheme is one we
    // recognise (`http`, `https`, `file`, `ftp`, `data`, `about`,
    // `chrome`, `view-source`). Anything else falls through to the
    // host-shape heuristic in branch 2.
    if let Ok(parsed) = url::Url::parse(trimmed)
        && is_real_scheme(parsed.scheme())
    {
        return parsed.to_string();
    }

    // Branch 2: scheme-less but URL-shaped.
    if looks_like_url(trimmed) {
        let prefix = if needs_http(trimmed) { "http" } else { "https" };
        return format!("{prefix}://{trimmed}");
    }

    // Branch 3: search-engine fallback.
    let template = search
        .engines
        .get(&search.default_engine)
        .map(|e| e.url.as_str())
        .unwrap_or("https://duckduckgo.com/?q={query}");
    template.replace("{query}", &url_encode(trimmed))
}

/// Heuristic for "scheme-less URL". Matches:
///
/// - `host.tld` (any host with at least one dot, no whitespace)
/// - `host.tld:port`
/// - `host.tld/path` and `host.tld:port/path`
/// - `localhost`, `localhost:3000`, `localhost/foo`
/// - bare IPv4: `192.168.1.1`, `10.0.0.1:8080`
///
/// Rejects:
///
/// - Anything with whitespace.
/// - Bare single-word strings without a dot (e.g. `foobar`).
/// - Strings that already start with a scheme (caller handled).
fn looks_like_url(s: &str) -> bool {
    if s.contains(char::is_whitespace) {
        return false;
    }
    // Split off path portion at the first `/` (if any) and any
    // query/fragment, then validate the host[:port] head.
    let head = s.split(['/', '?', '#']).next().unwrap_or("");
    if head.is_empty() {
        return false;
    }
    // Strip optional `:port`.
    let (host, port) = match head.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => (h, Some(p)),
        _ => (head, None),
    };
    if host.is_empty() {
        return false;
    }
    // Special-case `localhost`.
    if host.eq_ignore_ascii_case("localhost") {
        return port.is_some() || port.is_none();
    }
    // IPv4 dotted quad.
    if is_ipv4(host) {
        return true;
    }
    // Generic hostname: must have at least one `.`, every label
    // alphanumeric or `-`, no leading/trailing `-` per label.
    if !host.contains('.') {
        return false;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    })
}

/// Should the scheme-less URL get `http://` rather than `https://`?
///
/// Convention: localhost and IPv4 addresses default to `http`; public
/// hostnames default to `https`. This matches Chrome's address bar
/// heuristic well enough that pasting `localhost:3000` lands on a dev
/// server without TLS errors.
fn needs_http(s: &str) -> bool {
    let head = s.split(['/', '?', '#']).next().unwrap_or("");
    let host = match head.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => h,
        _ => head,
    };
    host.eq_ignore_ascii_case("localhost") || is_ipv4(host)
}

/// Schemes we treat as fully-qualified URLs in branch 1. Anything
/// outside this list (including the false-positive "scheme" parse of
/// `localhost:3000` as scheme=`localhost`) drops through to the
/// host-shape heuristic.
fn is_real_scheme(s: &str) -> bool {
    matches!(
        s,
        "http"
            | "https"
            | "file"
            | "ftp"
            | "ftps"
            | "data"
            | "about"
            | "chrome"
            | "view-source"
            | "javascript"
            | "mailto"
    )
}

fn is_ipv4(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() != 4 {
        return false;
    }
    parts
        .iter()
        .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) && p.parse::<u8>().is_ok())
}

/// Minimal `application/x-www-form-urlencoded`-style encoder. We
/// avoid pulling `percent-encoding` for one call site — the rules
/// here are: ASCII letters/digits and `-_.~` pass through, space →
/// `+`, everything else → `%XX` upper-hex.
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push('+'),
            _ => {
                out.push('%');
                out.push_str(&format!("{b:02X}"));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Search;

    fn search() -> Search {
        Search::default()
    }

    #[test]
    fn full_url_passes_through() {
        let s = search();
        assert_eq!(
            resolve_input("https://example.com", &s),
            "https://example.com/"
        );
    }

    #[test]
    fn http_scheme_preserved() {
        let s = search();
        assert_eq!(
            resolve_input("http://example.com/path", &s),
            "http://example.com/path"
        );
    }

    #[test]
    fn schemeless_host_gets_https() {
        let s = search();
        assert_eq!(resolve_input("example.com", &s), "https://example.com");
    }

    #[test]
    fn schemeless_host_with_path() {
        let s = search();
        assert_eq!(
            resolve_input("example.com/path", &s),
            "https://example.com/path"
        );
    }

    #[test]
    fn schemeless_host_with_port_gets_https_unless_localhost() {
        let s = search();
        assert_eq!(
            resolve_input("example.com:8443", &s),
            "https://example.com:8443"
        );
    }

    #[test]
    fn localhost_gets_http() {
        let s = search();
        assert_eq!(resolve_input("localhost:3000", &s), "http://localhost:3000");
        assert_eq!(resolve_input("localhost", &s), "http://localhost");
        assert_eq!(resolve_input("localhost/path", &s), "http://localhost/path");
    }

    #[test]
    fn ipv4_gets_http() {
        let s = search();
        assert_eq!(resolve_input("192.168.1.1", &s), "http://192.168.1.1");
        assert_eq!(resolve_input("10.0.0.1:8080", &s), "http://10.0.0.1:8080");
    }

    #[test]
    fn space_separated_query_routes_to_search() {
        let s = search();
        let resolved = resolve_input("foo bar", &s);
        assert_eq!(resolved, "https://duckduckgo.com/?q=foo+bar");
    }

    #[test]
    fn single_word_without_dot_is_search() {
        let s = search();
        let resolved = resolve_input("foobar", &s);
        assert_eq!(resolved, "https://duckduckgo.com/?q=foobar");
    }

    #[test]
    fn empty_input_returns_empty() {
        let s = search();
        assert_eq!(resolve_input("", &s), "");
        assert_eq!(resolve_input("   ", &s), "");
    }

    #[test]
    fn url_encodes_special_chars_in_query() {
        let s = search();
        // Query with reserved chars that must be percent-encoded.
        let resolved = resolve_input("a&b=c", &s);
        assert_eq!(resolved, "https://duckduckgo.com/?q=a%26b%3Dc");
    }

    #[test]
    fn url_with_query_string_passes_through() {
        let s = search();
        assert_eq!(
            resolve_input("https://example.com/?q=test", &s),
            "https://example.com/?q=test"
        );
    }

    #[test]
    fn file_scheme_passes_through() {
        let s = search();
        assert_eq!(resolve_input("file:///etc/hosts", &s), "file:///etc/hosts");
    }

    #[test]
    fn whitespace_trimmed_before_resolution() {
        let s = search();
        assert_eq!(resolve_input("  example.com  ", &s), "https://example.com");
    }

    #[test]
    fn bad_default_engine_falls_back_to_ddg() {
        let s = Search {
            default_engine: "missing".into(),
            engines: std::collections::HashMap::new(),
        };
        let resolved = resolve_input("hello", &s);
        assert_eq!(resolved, "https://duckduckgo.com/?q=hello");
    }

    #[test]
    fn looks_like_url_basic() {
        assert!(looks_like_url("example.com"));
        assert!(looks_like_url("example.com/path"));
        assert!(looks_like_url("example.com:8080"));
        assert!(looks_like_url("localhost"));
        assert!(looks_like_url("localhost:3000"));
        assert!(looks_like_url("192.168.1.1"));
        assert!(!looks_like_url("foobar"));
        assert!(!looks_like_url("foo bar"));
        assert!(!looks_like_url(""));
    }

    #[test]
    fn url_encode_basic() {
        assert_eq!(url_encode("hello"), "hello");
        assert_eq!(url_encode("hello world"), "hello+world");
        assert_eq!(url_encode("a&b"), "a%26b");
        assert_eq!(url_encode("a-b_c.d~e"), "a-b_c.d~e");
    }
}
