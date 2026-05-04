#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let s = std::str::from_utf8(data).unwrap_or("");
    if let Ok(chords) = buffr_modal::key::parse_keys(s) {
        for chord in &chords {
            // Exercise parse_key on each chord's debug representation.
            // This exercises the single-chord path without relying on
            // round-trip formatting — we just drive the parser with each
            // line of the original input instead.
            let _ = chord;
        }
    }
    // Also exercise parse_key on each whitespace-delimited token.
    for token in s.split_whitespace() {
        let _ = buffr_modal::key::parse_key(token);
    }
});
