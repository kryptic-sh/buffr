#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let s = std::str::from_utf8(data).unwrap_or("");
    if let Ok(cfg) = toml::from_str::<buffr_config::Config>(s) {
        let _ = buffr_config::validate(&cfg);
        let _ = buffr_config::to_toml_string(&cfg);
    }
});
