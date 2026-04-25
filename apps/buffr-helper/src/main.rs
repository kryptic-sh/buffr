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

use buffr_core::{BuffrApp, init_cef_api};

fn main() {
    // macOS only: load `Chromium Embedded Framework.framework` via the
    // cef-rs `LibraryLoader` before any CEF call. The helper binary
    // lives at `buffr.app/Contents/Frameworks/buffr Helper.app/Contents/MacOS/`,
    // so the loader resolves the framework via `../../..`. Linux/Windows
    // builds link CEF dynamically through `build.rs` and skip this.
    #[cfg(target_os = "macos")]
    {
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(err) => {
                eprintln!("buffr-helper: resolving current_exe failed: {err}");
                std::process::exit(1);
            }
        };
        let loader = cef::library_loader::LibraryLoader::new(&exe, true);
        if !loader.load() {
            eprintln!("buffr-helper: failed to load CEF framework via LibraryLoader");
            std::process::exit(1);
        }
        // Keep the loader alive for the lifetime of the process —
        // `Drop` calls `unload_library`, which we only want at exit.
        std::mem::forget(loader);
    }

    // Pin the CEF API version before touching any CEF entry — see
    // `buffr_core::init_cef_api` for the gory details. The helper
    // hits `execute_process` directly, so the call must happen first.
    init_cef_api();

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
