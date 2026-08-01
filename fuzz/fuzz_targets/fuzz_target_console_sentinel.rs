#![no_main]

//! Fuzz the three renderer → browser console-sentinel parsers.
//!
//! These are the most directly attacker-reachable parsers in the tree:
//! any page can emit `console.log("__buffr_hint__:" + anything)` and the
//! string lands in `parse_console_event` verbatim. All three sit on the
//! shared `console_sentinel::parse_sentinel` helper, so driving the same
//! `&str` through each of them also exercises the helper's byte-index
//! slicing (`&line[idx + sentinel.len()..]`) against inputs where the
//! sentinel is adjacent to a multi-byte UTF-8 boundary.

use libfuzzer_sys::fuzz_target;

use buffr_core::{edit, hint, media_probe};

fn drive(s: &str) {
    let _ = hint::parse_console_event(s);
    let _ = edit::parse_console_event(s);
    let _ = media_probe::parse(s);
}

fuzz_target!(|data: &[u8]| {
    // Raw bytes, best-effort as UTF-8: mirrors what CEF hands us for a
    // console message (already a Rust `String` by the time it reaches
    // the parsers, so lossless UTF-8 is the realistic shape).
    let Ok(s) = std::str::from_utf8(data) else {
        return;
    };
    drive(s);

    // Prefix each sentinel explicitly so the fuzzer doesn't have to
    // rediscover the 15-byte magic before it reaches the JSON decoders.
    // Without this nearly every input bails at the `find` in
    // `sentinel_payload` and the interesting half never runs.
    for sentinel in [
        hint::HINT_CONSOLE_SENTINEL,
        edit::EDIT_CONSOLE_SENTINEL,
        media_probe::MEDIA_PROBE_SENTINEL,
    ] {
        let mut line = String::with_capacity(sentinel.len() + s.len());
        line.push_str(sentinel);
        line.push_str(s);
        drive(&line);
    }
});
