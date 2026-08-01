#![no_main]

//! Fuzz the three renderer → browser console-sentinel parsers.
//!
//! These are the most directly attacker-reachable parsers in the tree:
//! any page can emit `console.log("__buffr_hint__:" + anything)` and the
//! string lands in `parse_console_event` verbatim. All three sit on the
//! shared `console_sentinel::parse_sentinel` helper, so driving the same
//! `&str` through each of them also exercises the helper's prefix
//! stripping against inputs where the sentinel or the nonce is adjacent
//! to a multi-byte UTF-8 boundary.
//!
//! The wire format is `<sentinel><nonce>:<json>` — see
//! `buffr_core::console_nonce`. A fixed nonce is used here so the
//! fuzzer reaches the JSON decoders; the *reject* path is covered too,
//! by driving every input with a deliberately wrong nonce as well.

use libfuzzer_sys::fuzz_target;

use buffr_core::{edit, hint, media_probe};

/// Stand-in for a live per-load nonce. Any fixed 32-hex-char value works
/// — the parsers only ever compare it for equality.
const NONCE: &str = "0123456789abcdef0123456789abcdef";

/// Same shape, different value: every accept path must reject this.
const WRONG_NONCE: &str = "fedcba9876543210fedcba9876543210";

fn drive(s: &str, nonce: &str) {
    let _ = hint::parse_console_event(s, nonce);
    let _ = edit::parse_console_event(s, nonce);
    let _ = media_probe::parse(s, nonce);
}

fuzz_target!(|data: &[u8]| {
    // Raw bytes, best-effort as UTF-8: mirrors what CEF hands us for a
    // console message (already a Rust `String` by the time it reaches
    // the parsers, so lossless UTF-8 is the realistic shape).
    let Ok(s) = std::str::from_utf8(data) else {
        return;
    };
    drive(s, NONCE);

    // Prefix each sentinel and the nonce explicitly so the fuzzer doesn't
    // have to rediscover the magic before it reaches the JSON decoders.
    // Without this nearly every input bails at the anchored prefix check
    // in `sentinel_payload` and the interesting half never runs.
    for sentinel in [
        hint::HINT_CONSOLE_SENTINEL,
        edit::EDIT_CONSOLE_SENTINEL,
        media_probe::MEDIA_PROBE_SENTINEL,
    ] {
        let mut line = String::with_capacity(sentinel.len() + NONCE.len() + 1 + s.len());
        line.push_str(sentinel);
        line.push_str(NONCE);
        line.push(':');
        line.push_str(s);

        // Authentic line: exercises the decoders.
        drive(&line, NONCE);
        // Same line checked against a different nonce: exercises the
        // reject path, which must never reach a decoder.
        drive(&line, WRONG_NONCE);
        // Empty expected nonce must fail closed.
        drive(&line, "");
    }
});
