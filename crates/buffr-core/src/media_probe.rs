//! Media-activity console-log IPC reader.
//!
//! Companion to `assets/media_probe_poll.js` which writes a sentinel-prefixed
//! line on every transition:
//!
//! ```text
//! __buffr_media__:<nonce>:{"media":true,"video":false}
//! ```
//!
//! [`BuffrDisplayHandler::on_console_message`] calls [`parse`] on every
//! console line; matched events flip the `video_active` / `media_active`
//! atomics on [`BrowserHost`]. Lines that are not ours — or that carry the
//! wrong nonce — return `None` so the caller can fall through to other
//! scrapers (edit / hint).
//!
//! The `<nonce>` is minted per main-frame load (see
//! [`crate::console_nonce`]) and spliced into the poll script by
//! [`build_poll_script`]. Without it, any frame on the page — an ad iframe
//! included — could emit `__buffr_media__:{"video":true}` in a loop and pin
//! the platform idle inhibitor on so the user's screen never locks.
//!
//! Same shape as [`crate::edit::parse_console_event`] and
//! [`crate::hint::parse_console_event`] — see those for the wider pattern.

use serde::Deserialize;

/// Sentinel prefix written by `media_probe_poll.js`.
pub const MEDIA_PROBE_SENTINEL: &str = "__buffr_media__:";

/// Decoded payload: snapshot of the JS probe's flags as of the emit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct MediaProbeEvent {
    /// Any media (audio OR video) signal active.
    pub media: bool,
    /// Video-specific signal active (video element / fullscreen video /
    /// WebRTC video track / wakelock / mediaSession).
    pub video: bool,
}

/// Try to parse a console line as a media-probe event.
///
/// `nonce` is the page nonce currently minted for the emitting browser
/// (`ConsoleNonces::page`).
///
/// - `None` — line is not an authentic [`MEDIA_PROBE_SENTINEL`] line for
///   `nonce` (absent sentinel, wrong nonce, or not anchored at the start).
/// - `Some(Ok(event))` — authentic, JSON decoded.
/// - `Some(Err(err))` — authentic but JSON decode failed.
pub fn parse(line: &str, nonce: &str) -> Option<Result<MediaProbeEvent, serde_json::Error>> {
    crate::console_sentinel::parse_sentinel(line, MEDIA_PROBE_SENTINEL, nonce)
}

/// Build the media-probe poll script with `nonce` spliced in.
///
/// Substitutes the one placeholder the asset uses:
///
/// - `%%SENTINEL%%` → [`MEDIA_PROBE_SENTINEL`] + `nonce` + `:`
///
/// The asset already wraps the substitution site in a string literal, and
/// both halves are ASCII, so no extra quoting is needed.
pub fn build_poll_script(nonce: &str) -> String {
    crate::scripts::MEDIA_PROBE_POLL_JS_TEMPLATE.replace(
        "%%SENTINEL%%",
        &crate::console_sentinel::sentinel_prefix(MEDIA_PROBE_SENTINEL, nonce),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONCE: &str = "0123456789abcdef0123456789abcdef";

    fn line(body: &str) -> String {
        format!("{MEDIA_PROBE_SENTINEL}{NONCE}:{body}")
    }

    #[test]
    fn parses_video_active() {
        let event = parse(&line(r#"{"media":true,"video":true}"#), NONCE)
            .unwrap()
            .unwrap();
        assert!(event.media);
        assert!(event.video);
    }

    #[test]
    fn parses_audio_only() {
        let event = parse(&line(r#"{"media":true,"video":false}"#), NONCE)
            .unwrap()
            .unwrap();
        assert!(event.media);
        assert!(!event.video);
    }

    #[test]
    fn parses_idle() {
        let event = parse(&line(r#"{"media":false,"video":false}"#), NONCE)
            .unwrap()
            .unwrap();
        assert!(!event.media);
        assert!(!event.video);
    }

    #[test]
    fn ignores_non_sentinel() {
        assert!(parse("hello world", NONCE).is_none());
        assert!(parse(&format!("__buffr_edit__:{NONCE}:{{}}"), NONCE).is_none());
    }

    #[test]
    fn rejects_forged_line_without_nonce() {
        // H5: the pre-nonce wire format. Any frame could emit this to pin
        // the idle inhibitor on.
        assert!(parse(r#"__buffr_media__:{"media":true,"video":true}"#, NONCE).is_none());
    }

    #[test]
    fn rejects_forged_line_with_wrong_nonce() {
        let forged = format!(
            "{MEDIA_PROBE_SENTINEL}{}:{}",
            "f".repeat(32),
            r#"{"media":true,"video":true}"#
        );
        assert!(parse(&forged, NONCE).is_none());
    }

    #[test]
    fn rejects_sentinel_after_format_prefix() {
        // Anchored parse (H5): a page-supplied prefix no longer smuggles a
        // payload through, even when the nonce is right.
        let forged = format!("%cINFO {}", line(r#"{"media":true,"video":true}"#));
        assert!(parse(&forged, NONCE).is_none());
    }

    #[test]
    fn rejects_malformed_json() {
        let res = parse(&line("{not json"), NONCE).unwrap();
        assert!(res.is_err());
    }

    #[test]
    fn poll_script_carries_the_nonce_and_no_placeholder() {
        let script = build_poll_script(NONCE);
        assert!(
            !script.contains("%%SENTINEL%%"),
            "%%SENTINEL%% not substituted"
        );
        assert!(script.contains(&format!("{MEDIA_PROBE_SENTINEL}{NONCE}:")));
    }

    #[test]
    fn poll_script_output_round_trips_through_parse() {
        // The emitted prefix in the script must be exactly what `parse`
        // accepts — the two halves of the protocol cannot drift.
        let script = build_poll_script(NONCE);
        let prefix = format!("{MEDIA_PROBE_SENTINEL}{NONCE}:");
        assert!(script.contains(&prefix));
        let emitted = format!("{prefix}{}", r#"{"media":true,"video":false}"#);
        assert!(parse(&emitted, NONCE).unwrap().is_ok());
    }

    #[test]
    fn scripts_for_two_loads_differ() {
        use crate::console_nonce::new_console_nonce;
        let a = build_poll_script(&new_console_nonce());
        let b = build_poll_script(&new_console_nonce());
        assert_ne!(a, b, "nonce must change across page loads");
    }

    #[test]
    fn wire_line_decodes_media_and_video_flags() {
        let ev = parse(&line(r#"{"media":true,"video":false}"#), NONCE)
            .unwrap()
            .unwrap();
        assert!(ev.media);
        assert!(!ev.video);
    }
}
