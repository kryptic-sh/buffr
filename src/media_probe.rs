//! Media-activity console-log IPC reader.
//!
//! Companion to `assets/media_probe_poll.js` which writes a sentinel-prefixed
//! line on every transition:
//!
//! ```text
//! __buffr_media__:{"media":true,"video":false}
//! ```
//!
//! [`BuffrDisplayHandler::on_console_message`] calls [`parse`] on every
//! console line; matched events flip the `video_active` / `media_active`
//! atomics on [`BrowserHost`]. Non-sentinel lines return `None` so the caller
//! can fall through to other scrapers (edit / hint).
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
/// - `None` — line does not carry the [`MEDIA_PROBE_SENTINEL`] prefix.
/// - `Some(Ok(event))` — prefix present, JSON decoded.
/// - `Some(Err(err))` — prefix present but JSON decode failed.
pub fn parse(line: &str) -> Option<Result<MediaProbeEvent, serde_json::Error>> {
    // Some pages wrap `console.log` to prepend a styling format string
    // (e.g. `%cINFO ...`); locate the sentinel anywhere in the line, not
    // just at the start. Mirrors the edit.rs / hint.rs handling.
    let idx = line.find(MEDIA_PROBE_SENTINEL)?;
    let suffix = &line[idx + MEDIA_PROBE_SENTINEL.len()..];
    Some(serde_json::from_str::<MediaProbeEvent>(suffix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_video_active() {
        let event = parse(r#"__buffr_media__:{"media":true,"video":true}"#)
            .unwrap()
            .unwrap();
        assert!(event.media);
        assert!(event.video);
    }

    #[test]
    fn parses_audio_only() {
        let event = parse(r#"__buffr_media__:{"media":true,"video":false}"#)
            .unwrap()
            .unwrap();
        assert!(event.media);
        assert!(!event.video);
    }

    #[test]
    fn parses_idle() {
        let event = parse(r#"__buffr_media__:{"media":false,"video":false}"#)
            .unwrap()
            .unwrap();
        assert!(!event.media);
        assert!(!event.video);
    }

    #[test]
    fn ignores_non_sentinel() {
        assert!(parse("hello world").is_none());
        assert!(parse("__buffr_edit__:{}").is_none());
    }

    #[test]
    fn finds_sentinel_after_format_prefix() {
        // monkeytype-style page wraps console.log with a format string.
        let event = parse(r#"%cINFO __buffr_media__:{"media":true,"video":true}"#)
            .unwrap()
            .unwrap();
        assert!(event.video);
    }

    #[test]
    fn rejects_malformed_json() {
        let res = parse(r#"__buffr_media__:{not json"#).unwrap();
        assert!(res.is_err());
    }
}
