#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let s = std::str::from_utf8(data).unwrap_or("");
    // Exercise parse_key on each whitespace-delimited token.
    for token in s.split_whitespace() {
        let _ = buffr_modal::key::parse_key(token);
    }
});
