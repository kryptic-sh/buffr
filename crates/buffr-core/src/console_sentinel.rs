//! Shared plumbing for the renderer → browser console-log IPC.
//!
//! Three subsystems (`hint`, `edit`, `media_probe`) each ship a JS
//! snippet that emits `console.log("<sentinel><nonce>:" + JSON.stringify(…))`,
//! and each has a `parse_*` entry point on the Rust side. The
//! sentinel-locating and JSON-decoding halves are identical, so they
//! live here once: any change to the wire format lands in a single
//! place instead of three.
//!
//! # Wire format
//!
//! ```text
//! __buffr_edit__:9f3c…a1:{"type":"blur","field_id":"f3"}
//! ^ sentinel      ^ nonce  ^ JSON payload
//! ```
//!
//! Two hardening rules, both load-bearing (see [`crate::console_nonce`]
//! for the threat model):
//!
//! 1. **Anchored.** The match is at the *start* of the console line.
//!    Previously the sentinel was located with `str::find` anywhere in the
//!    line, so a page could hide a payload behind arbitrary leading text.
//!    The cost is that a page which wraps `console.log` to prepend its own
//!    format string (`%cINFO …`) now hides our own payload too — an
//!    availability regression on such pages, deliberately accepted.
//! 2. **Nonce-checked.** The nonce must equal the one currently minted for
//!    the emitting browser. Frames we never injected into (subframes, ad
//!    iframes) have no way to know it.

use serde::de::DeserializeOwned;

use crate::console_nonce::CONSOLE_NONCE_SEPARATOR;

/// Strip `sentinel` + `nonce` + separator from the *front* of `line` and
/// return the remainder.
///
/// Returns `None` when the line does not start with the sentinel, when the
/// nonce does not match, or when `nonce` is empty (fail closed — an empty
/// expected nonce must never authorise anything).
pub(crate) fn sentinel_payload<'a>(line: &'a str, sentinel: &str, nonce: &str) -> Option<&'a str> {
    if nonce.is_empty() {
        return None;
    }
    let rest = line.strip_prefix(sentinel)?;
    let rest = rest.strip_prefix(nonce)?;
    rest.strip_prefix(CONSOLE_NONCE_SEPARATOR)
}

/// The exact prefix an injected script must emit for `sentinel` / `nonce`.
///
/// Spliced into the JS assets by the `build_*_script` helpers so the two
/// halves of the protocol cannot drift apart.
pub(crate) fn sentinel_prefix(sentinel: &str, nonce: &str) -> String {
    format!("{sentinel}{nonce}{CONSOLE_NONCE_SEPARATOR}")
}

/// [`sentinel_payload`] plus a `serde_json` decode of the tail.
///
/// - `None` — the line is not an authentic sentinel line for `nonce`; the
///   caller should fall through to its other console scrapers. This covers
///   both "not ours" and "forged" — a forged line is indistinguishable from
///   page chatter by design, and logging it would hand any page a way to
///   spam buffr's log.
/// - `Some(Err(_))` — the sentinel *and* the nonce matched but the JSON tail
///   would not parse. Deliberately distinct from `None` so callers can log
///   malformed output from our own scripts.
pub(crate) fn parse_sentinel<T: DeserializeOwned>(
    line: &str,
    sentinel: &str,
    nonce: &str,
) -> Option<Result<T, serde_json::Error>> {
    let suffix = sentinel_payload(line, sentinel, nonce)?;
    Some(serde_json::from_str::<T>(suffix))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    const NONCE: &str = "0123456789abcdef0123456789abcdef";

    #[derive(Debug, PartialEq, Deserialize)]
    struct Probe {
        a: u32,
    }

    fn line(body: &str) -> String {
        format!("__s__:{NONCE}:{body}")
    }

    #[test]
    fn missing_sentinel_is_none() {
        assert!(parse_sentinel::<Probe>("nothing here", "__s__:", NONCE).is_none());
    }

    #[test]
    fn decodes_tail() {
        let got = parse_sentinel::<Probe>(&line(r#"{"a":7}"#), "__s__:", NONCE)
            .unwrap()
            .unwrap();
        assert_eq!(got, Probe { a: 7 });
    }

    #[test]
    fn rejects_missing_nonce() {
        assert!(parse_sentinel::<Probe>(r#"__s__:{"a":1}"#, "__s__:", NONCE).is_none());
    }

    #[test]
    fn rejects_wrong_nonce() {
        let forged = format!("__s__:{}:{}", "f".repeat(32), r#"{"a":1}"#);
        assert!(parse_sentinel::<Probe>(&forged, "__s__:", NONCE).is_none());
    }

    #[test]
    fn rejects_empty_expected_nonce() {
        // Fail closed: an empty expected nonce must not turn `sentinel:`
        // into a valid prefix.
        assert!(sentinel_payload("__s__::{}", "__s__:", "").is_none());
    }

    #[test]
    fn rejects_truncated_nonce() {
        let short = &NONCE[..NONCE.len() - 1];
        let forged = format!("__s__:{short}:{}", r#"{"a":1}"#);
        assert!(parse_sentinel::<Probe>(&forged, "__s__:", NONCE).is_none());
    }

    #[test]
    fn rejects_sentinel_mid_line() {
        // Anchored parse: a page prefixing our payload no longer works.
        let forged = format!("%cINFO {}", line(r#"{"a":1}"#));
        assert!(parse_sentinel::<Probe>(&forged, "__s__:", NONCE).is_none());
    }

    #[test]
    fn rejects_leading_whitespace() {
        let forged = format!(" {}", line(r#"{"a":1}"#));
        assert!(parse_sentinel::<Probe>(&forged, "__s__:", NONCE).is_none());
    }

    #[test]
    fn present_but_malformed_is_some_err() {
        assert!(
            parse_sentinel::<Probe>(&line("{not json"), "__s__:", NONCE)
                .unwrap()
                .is_err()
        );
    }

    #[test]
    fn payload_can_be_empty() {
        assert_eq!(
            sentinel_payload(&format!("__s__:{NONCE}:"), "__s__:", NONCE),
            Some("")
        );
    }

    #[test]
    fn prefix_round_trips() {
        let prefix = sentinel_prefix("__s__:", NONCE);
        assert_eq!(prefix, format!("__s__:{NONCE}:"));
        assert_eq!(
            sentinel_payload(&format!("{prefix}tail"), "__s__:", NONCE),
            Some("tail")
        );
    }

    #[test]
    fn multibyte_tail_does_not_panic() {
        // The old implementation sliced by byte index; keep a regression
        // test that non-ASCII immediately after the separator is fine.
        let s = format!("__s__:{NONCE}:🦀");
        assert_eq!(sentinel_payload(&s, "__s__:", NONCE), Some("🦀"));
    }
}
