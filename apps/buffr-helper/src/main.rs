//! CEF subprocess helper.
//!
//! On Linux/Windows the main `buffr` binary re-launches itself with
//! `--type=...` for renderer / GPU / utility processes; we still ship
//! a separate `buffr-helper` binary for the macOS Helper.app bundle
//! path (CEF requires a distinct executable under `Contents/Frameworks`
//! on macOS).
//!
//! In all cases the helper does the bare minimum: forwards argv to
//! `buffr_cef::execute_subprocess`, exits with whatever code CEF returns.

fn main() {
    // Load the CEF framework library before any CEF call. On macOS this
    // resolves `Chromium Embedded Framework.framework` via `../../..`
    // (helper = true). On Linux/Windows this is a no-op.
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(err) => {
            eprintln!("buffr-helper: resolving current_exe failed: {err}");
            std::process::exit(1);
        }
    };
    if let Err(err) = buffr_cef::load_cef_library(&exe, true) {
        eprintln!("buffr-helper: {err}");
        std::process::exit(1);
    }

    // Returns >= 0 for child processes (renderer/GPU/utility) which exit
    // immediately afterwards; returns -1 for the browser process, which
    // never reaches a helper binary in practice.
    let exit = buffr_cef::execute_subprocess();
    std::process::exit(exit);
}
