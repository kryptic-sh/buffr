//! Shared plumbing for the renderer → browser console-log IPC.
//!
//! Three subsystems (`hint`, `edit`, `media_probe`) each ship a JS
//! snippet that emits `console.log("<sentinel>" + JSON.stringify(…))`,
//! and each has a `parse_*` entry point on the Rust side. The
//! sentinel-locating and JSON-decoding halves are identical, so they
//! live here once: any change to the wire format (an origin nonce, an
//! anchored match) lands in a single place instead of three.

use serde::de::DeserializeOwned;

/// Locate `sentinel` in `line` and return everything after it.
///
/// Returns `None` when the sentinel is absent. The match is *anywhere*
/// in the line rather than anchored at the start because some pages
/// wrap `console.log` to prepend their own styling format string
/// (`%cINFO …`), which would otherwise hide our payload.
pub(crate) fn sentinel_payload<'a>(line: &'a str, sentinel: &str) -> Option<&'a str> {
    let idx = line.find(sentinel)?;
    Some(&line[idx + sentinel.len()..])
}

/// [`sentinel_payload`] plus a `serde_json` decode of the tail.
///
/// - `None` — the line does not carry `sentinel`; the caller should
///   fall through to its other console scrapers.
/// - `Some(Err(_))` — the sentinel was present but the JSON tail would
///   not parse. Deliberately distinct from `None` so callers can log
///   malformed renderer output instead of silently dropping it.
pub(crate) fn parse_sentinel<T: DeserializeOwned>(
    line: &str,
    sentinel: &str,
) -> Option<Result<T, serde_json::Error>> {
    let suffix = sentinel_payload(line, sentinel)?;
    Some(serde_json::from_str::<T>(suffix))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, PartialEq, Deserialize)]
    struct Probe {
        a: u32,
    }

    #[test]
    fn missing_sentinel_is_none() {
        assert!(parse_sentinel::<Probe>("nothing here", "__s__:").is_none());
    }

    #[test]
    fn decodes_tail() {
        let got = parse_sentinel::<Probe>(r#"__s__:{"a":7}"#, "__s__:")
            .unwrap()
            .unwrap();
        assert_eq!(got, Probe { a: 7 });
    }

    #[test]
    fn finds_sentinel_mid_line() {
        let got = parse_sentinel::<Probe>(r#"%cINFO __s__:{"a":1}"#, "__s__:")
            .unwrap()
            .unwrap();
        assert_eq!(got, Probe { a: 1 });
    }

    #[test]
    fn present_but_malformed_is_some_err() {
        assert!(
            parse_sentinel::<Probe>("__s__:{not json", "__s__:")
                .unwrap()
                .is_err()
        );
    }

    #[test]
    fn payload_can_be_empty() {
        assert_eq!(sentinel_payload("__s__:", "__s__:"), Some(""));
    }
}
