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
use std::io::{Cursor, Read};
use std::time::Duration;

const FETCH_TIMEOUT_CONNECT: Duration = Duration::from_secs(5);
const FETCH_TIMEOUT_READ: Duration = Duration::from_secs(15);
const USER_AGENT: &str = concat!("buffr/", env!("CARGO_PKG_VERSION"));

/// Maximum bytes accepted from an image source before refusing (16 MiB,
/// generous for images). Applies to both http(s) bodies and decoded `data:`
/// payloads (A9).
const IMAGE_FETCH_MAX_BYTES: usize = 16 * 1024 * 1024;

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
    check_fetch_host(url)?;
    let config = ureq::Agent::config_builder()
        .timeout_connect(Some(FETCH_TIMEOUT_CONNECT))
        .timeout_recv_response(Some(FETCH_TIMEOUT_READ))
        .user_agent(USER_AGENT)
        .build();
    let agent = ureq::Agent::new_with_config(config);
    let mut resp = agent.get(url).call().map_err(|e| format!("fetch: {e}"))?;
    let mut body = Vec::new();
    resp.body_mut()
        .as_reader()
        .take(IMAGE_FETCH_MAX_BYTES as u64 + 1)
        .read_to_end(&mut body)
        .map_err(|e| format!("body read: {e}"))?;
    if body.len() > IMAGE_FETCH_MAX_BYTES {
        return Err("image response too large".into());
    }
    Ok(body)
}

/// Reject `http(s)` URLs whose host is loopback, private, link-local or
/// numeric-shaped (A9). Copy Image runs in the browser process, so it must
/// not pivot a page's request into the local network (the page cannot
/// fetch loopback itself — CORS blocks it; buffr would be the proxy).
fn check_fetch_host(url: &str) -> Result<(), String> {
    let parsed = url::Url::parse(url).map_err(|e| format!("invalid URL: {e}"))?;
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(format!("unsupported scheme: {scheme}"));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| "URL has no host".to_string())?;
    // `host_str()` returns the bracketed serialization for IPv6 literals
    // (e.g. `[::1]`), but the shared guard's IPv6 path expects the
    // unbracketed form. Only IPv6 hosts ever carry brackets, so stripping
    // them is lossless.
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if crate::private_net::is_non_public_host(host)
        || !crate::private_net::host_resolves_public(host)
    {
        return Err(format!("refusing to fetch private-network host `{host}`"));
    }
    Ok(())
}

/// Decode the body of a `data:` URL (post-prefix). Accepts base64 and
/// percent-encoded payloads; ignores the media-type parameters.
fn decode_data_url(rest: &str) -> Result<Vec<u8>, String> {
    let (meta, payload) = rest
        .split_once(',')
        .ok_or_else(|| "data URL missing comma".to_string())?;
    let is_base64 = meta.split(';').any(|p| p.eq_ignore_ascii_case("base64"));
    if is_base64 {
        // base64 expands ~4/3; the +4 covers padding. Bound the encoded
        // payload before decoding so a huge `data:` URL can't allocate
        // unbounded memory (A9).
        if payload.len() > IMAGE_FETCH_MAX_BYTES * 4 / 3 + 4 {
            return Err("data: payload too large".into());
        }
        let bytes = B64
            .decode(payload.as_bytes())
            .map_err(|e| format!("base64 decode: {e}"))?;
        if bytes.len() > IMAGE_FETCH_MAX_BYTES {
            return Err("data: payload too large".into());
        }
        Ok(bytes)
    } else {
        // Percent-decoded payload — rare for images, but spec-legal.
        // Decoded size ≤ payload.len(), so bounding the payload bounds the
        // decode.
        if payload.len() > IMAGE_FETCH_MAX_BYTES {
            return Err("data: payload too large".into());
        }
        Ok(percent_decode(payload))
    }
}

/// Percent-decode `s` into **raw bytes**.
///
/// Must not go via `String`: a `data:` image payload is binary, and
/// mapping each decoded byte to a `char` would UTF-8-encode everything
/// ≥ 0x80 into two bytes (`%89` → `0xC2 0x89`), corrupting the PNG
/// magic and every other high byte. Undecodable `%` sequences are
/// passed through verbatim, matching browser behaviour.
fn percent_decode(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_digit(bytes[i + 1]), hex_digit(bytes[i + 2]))
        {
            out.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
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
        assert_eq!(percent_decode("a%20b%2Fc"), b"a b/c".to_vec());
    }

    #[test]
    fn percent_decode_preserves_high_bytes() {
        // Regression: decoding used to go via `String`, so every byte
        // ≥ 0x80 was re-encoded as two UTF-8 bytes.
        assert_eq!(percent_decode("%89%FF%00"), vec![0x89u8, 0xFF, 0x00]);
    }

    #[test]
    fn data_url_percent_encoded_keeps_png_magic() {
        // Non-base64 `data:` payload — the PNG signature must survive
        // byte-for-byte.
        let bytes = decode_data_url("image/png,%89PNG%0D%0A%1A%0A").unwrap();
        assert_eq!(bytes, b"\x89PNG\r\n\x1a\n".to_vec());
    }

    #[test]
    fn percent_decode_passes_through_bad_escapes() {
        assert_eq!(percent_decode("a%zzb%2"), b"a%zzb%2".to_vec());
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

    #[test]
    fn check_fetch_host_rejects_private_hosts() {
        for url in [
            "http://127.0.0.1/x",
            "http://localhost/x",
            "http://169.254.169.254/latest/meta-data/",
            "http://192.168.1.1/x",
            "http://10.0.0.5/x",
            // The integer form of the cloud-metadata endpoint: glibc's
            // getaddrinfo resolves it to 169.254.169.254.
            "http://2852039166/x",
            // Bracketed by `host_str()`; `check_fetch_host` strips them so
            // the shared guard's unbracketed IPv6 path sees `::1`.
            "http://[::1]/x",
        ] {
            assert!(check_fetch_host(url).is_err(), "{url} should be rejected");
        }
    }

    #[test]
    fn check_fetch_host_allows_public_hosts() {
        for url in ["https://8.8.8.8/img.png", "http://93.184.216.34/img.png"] {
            assert!(check_fetch_host(url).is_ok(), "{url} should be allowed");
        }
    }

    #[test]
    fn decode_data_url_rejects_oversized_payload() {
        // base64 of exactly `IMAGE_FETCH_MAX_BYTES` bytes encodes to
        // ceil(n/3)*4 = cap*4/3 rounded down — just under the pre-check
        // ceiling — so go modestly over (+3 bytes) to push the encoded
        // length past `cap*4/3 + 4`. ~16 MiB allocation, no decode.
        let payload = "x".repeat(IMAGE_FETCH_MAX_BYTES + 3);
        let rest = format!("image/png;base64,{}", B64.encode(payload.as_bytes()));
        assert!(decode_data_url(&rest).is_err());
    }
}
