//! Off-thread image-URL → PNG bytes → clipboard pipeline.
//!
//! Used by the right-click context menu's "Copy Image" item. We can't use
//! CEF's clipboard for image data on OSR (issue #19 — defers IMAGE-mime
//! paste, but outbound copy is fine via a parallel `hjkl-clipboard`
//! handle), so we fetch the image bytes ourselves, decode them via the
//! `image` crate, and re-encode to PNG for the system clipboard.
//!
//! Why re-encode unconditionally:
//! - `MimeType::Png` is the only image variant `hjkl-clipboard` exposes
//!   first-class; pasting into GIMP / Slack / browsers expects PNG bytes.
//! - JPEG / WebP / GIF round-trip through `image::DynamicImage` losslessly
//!   into PNG (we're not preserving artistic intent, we're moving pixels).
//! - One code path = fewer edge cases.
//!
//! Invocation: spawn-and-forget via [`copy_image_to_clipboard`] — the
//! caller drops the JoinHandle so the UI thread never blocks on network.
//! All errors are logged at WARN; there is no caller-visible failure path.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use hjkl_clipboard::{Clipboard, ClipboardError, MimeType, Selection};
use std::io::Cursor;
use std::time::Duration;

const FETCH_TIMEOUT_CONNECT: Duration = Duration::from_secs(5);
const FETCH_TIMEOUT_READ: Duration = Duration::from_secs(15);
const USER_AGENT: &str = concat!("buffr/", env!("CARGO_PKG_VERSION"));

/// Spawn an off-thread worker that fetches `url`, decodes it, and
/// pushes the PNG bytes to the system clipboard. The function returns
/// immediately; progress and outcome are reported via `tracing`.
///
/// On backends that don't carry image MIME (OSC52 over SSH, mock), we
/// fall back to copying the URL itself as text so the user gets
/// *something* useful instead of silence.
pub fn copy_image_to_clipboard(url: String) {
    if url.is_empty() {
        return;
    }
    std::thread::spawn(move || {
        if let Err(err) = run(&url) {
            tracing::warn!(error = %err, url = %url, "copy_image: failed");
        }
    });
}

fn run(url: &str) -> Result<(), String> {
    let cb = Clipboard::new().map_err(|e| format!("clipboard init: {e}"))?;
    let bytes = fetch_image_bytes(url)?;
    let png_bytes = transcode_to_png(&bytes)?;
    match cb.set(Selection::Clipboard, MimeType::Png, &png_bytes) {
        Ok(()) => {
            tracing::info!(
                url = %url,
                png_len = png_bytes.len(),
                "copy_image: success"
            );
            Ok(())
        }
        Err(ClipboardError::UnsupportedMime) => {
            // Backend can't carry images (OSC52, mock). Drop URL as text.
            cb.set(Selection::Clipboard, MimeType::Text, url.as_bytes())
                .map_err(|e| format!("text fallback set: {e}"))?;
            tracing::info!(
                url = %url,
                "copy_image: IMAGE mime unsupported; copied URL as text"
            );
            Ok(())
        }
        Err(err) => Err(format!("png set: {err}")),
    }
}

fn fetch_image_bytes(url: &str) -> Result<Vec<u8>, String> {
    if let Some(rest) = url.strip_prefix("data:") {
        return decode_data_url(rest);
    }
    if url.starts_with("blob:") {
        // CEF-internal blobs aren't reachable from outside the renderer —
        // see issue #19 for the wider OSR clipboard story.
        return Err("blob: URLs not supported".into());
    }
    let config = ureq::Agent::config_builder()
        .timeout_connect(Some(FETCH_TIMEOUT_CONNECT))
        .timeout_recv_response(Some(FETCH_TIMEOUT_READ))
        .user_agent(USER_AGENT)
        .build();
    let agent = ureq::Agent::new_with_config(config);
    let mut resp = agent.get(url).call().map_err(|e| format!("fetch: {e}"))?;
    resp.body_mut()
        .read_to_vec()
        .map_err(|e| format!("body read: {e}"))
}

/// Decode the body of a `data:` URL (post-prefix). Accepts base64 and
/// percent-encoded payloads; ignores the media-type parameters.
fn decode_data_url(rest: &str) -> Result<Vec<u8>, String> {
    let (meta, payload) = rest
        .split_once(',')
        .ok_or_else(|| "data URL missing comma".to_string())?;
    let is_base64 = meta.split(';').any(|p| p.eq_ignore_ascii_case("base64"));
    if is_base64 {
        B64.decode(payload.as_bytes())
            .map_err(|e| format!("base64 decode: {e}"))
    } else {
        // Percent-decoded text payload — rare for images, but spec-legal.
        Ok(percent_decode(payload).into_bytes())
    }
}

fn percent_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_digit(bytes[i + 1]), hex_digit(bytes[i + 2]))
        {
            out.push(((hi << 4) | lo) as char);
            i += 3;
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn transcode_to_png(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let img = image::load_from_memory(bytes).map_err(|e| format!("decode: {e}"))?;
    let mut out = Vec::with_capacity(bytes.len()); // approx; PNG may grow JPEG
    img.write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
        .map_err(|e| format!("png encode: {e}"))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_url_base64_round_trips() {
        // 1x1 red PNG.
        let png_b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
        let url = format!("image/png;base64,{png_b64}");
        let bytes = decode_data_url(&url).unwrap();
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn data_url_missing_comma_errors() {
        assert!(decode_data_url("image/png;base64NOPAYLOAD").is_err());
    }

    #[test]
    fn percent_decode_hex_pairs() {
        assert_eq!(percent_decode("a%20b%2Fc"), "a b/c");
    }

    #[test]
    fn transcode_jpeg_to_png() {
        // Tiny synthetic JPEG generated at test time.
        let img = image::DynamicImage::new_rgb8(2, 2);
        let mut jpeg = Vec::new();
        img.write_to(&mut Cursor::new(&mut jpeg), image::ImageFormat::Jpeg)
            .unwrap();
        let png = transcode_to_png(&jpeg).unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn transcode_invalid_bytes_errors() {
        assert!(transcode_to_png(&[0u8, 1, 2, 3]).is_err());
    }

    #[test]
    fn blob_url_rejected() {
        assert!(fetch_image_bytes("blob:https://example.com/abc").is_err());
    }
}
