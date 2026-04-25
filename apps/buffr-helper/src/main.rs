//! CEF subprocess helper.
//!
//! On Linux/Windows the main `buffr` binary re-launches itself with
//! `--type=...` for renderer / GPU / utility processes; we still ship
//! a separate `buffr-helper` binary for the macOS Helper.app bundle
//! path (CEF requires a distinct executable under `Contents/Frameworks`
//! on macOS).
//!
//! In all cases the helper does the bare minimum: forwards argv to
//! `cef::execute_process`, exits with whatever code CEF returns.

use buffr_core::BuffrApp;

fn main() {
    let args = cef::args::Args::new();
    let mut app = BuffrApp::new();

    // Returns >= 0 for child processes (renderer/GPU/utility) which
    // exit immediately afterwards; returns -1 for the browser process,
    // which never reaches a helper binary in practice.
    let code = cef::execute_process(
        Some(args.as_main_args()),
        Some(&mut app),
        std::ptr::null_mut(),
    );
    std::process::exit(code);
}
