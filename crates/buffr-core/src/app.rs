//! CEF `App` impl + per-user profile path resolution.
//!
//! The `cef` crate (147.x, tauri-apps/cef-rs) exposes the `App` trait
//! via its `wrap_app!` macro. We use it here to:
//!
//! - Override `on_before_command_line_processing` so we can inject our
//!   own Chromium command-line switches (feature flags, GPU/Wayland
//!   knobs, sandbox hints).
//! - Provide a `BrowserProcessHandler` that gets invoked once CEF is
//!   ready to create the first browser. For Phase 1 we don't actually
//!   create the browser from inside the handler — `apps/buffr` does
//!   that synchronously after `cef::initialize` from the main thread,
//!   driving its own winit event loop. The handler still exists so
//!   future phases can hook into context-init events without further
//!   plumbing.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

// `wrap_app!` / `wrap_browser_process_handler!` expand to references
// to bare `App`, `WrapApp`, `ImplApp`, `BrowserProcessHandler`, etc.
// — the upstream cefsimple example uses `use cef::*;` for this reason.
use cef::*;

use crate::new_tab::register_buffr_scheme;

/// Process-wide flag toggling the `--force-renderer-accessibility`
/// switch in `on_before_command_line_processing`. Set via
/// [`set_force_renderer_accessibility`] before the first `BuffrApp` is
/// constructed (i.e. before `cef::execute_process` / `cef::initialize`).
///
/// We use a static so the wrap_app! macro's struct doesn't need to
/// carry state — the cef-rs trait surface for `App` doesn't accept
/// per-instance fields cleanly.
static FORCE_RENDERER_ACCESSIBILITY: AtomicBool = AtomicBool::new(false);

/// Toggle the renderer accessibility tree for subsequent CEF launches.
/// Call before `BuffrApp::new()` if you want the switch picked up.
///
/// Backed by `--force-renderer-accessibility` (Chromium switch). cef-147
/// also exposes per-browser `SetAccessibilityState` on the host, but
/// the command-line switch is the only path that fires before any
/// browser exists — and it covers every renderer for the process.
pub fn set_force_renderer_accessibility(on: bool) {
    FORCE_RENDERER_ACCESSIBILITY.store(on, Ordering::SeqCst);
}

/// Read the current accessibility-flag toggle. Mostly useful for tests.
pub fn force_renderer_accessibility_enabled() -> bool {
    FORCE_RENDERER_ACCESSIBILITY.load(Ordering::SeqCst)
}

/// Resolved on-disk paths buffr uses for cache + profile data.
///
/// Populated by [`crate::profile_paths`].
#[derive(Debug, Clone)]
pub struct ProfilePaths {
    pub cache: PathBuf,
    pub data: PathBuf,
}

wrap_app! {
    // buffr's `cef::App` implementation.
    //
    // (Doc comments live outside this macro because `wrap_app!`'s
    // matcher doesn't accept `#[doc = "..."]` attributes on the
    // struct itself.)
    pub struct BuffrApp;

    impl App {
        fn on_register_custom_schemes(&self, registrar: Option<&mut SchemeRegistrar>) {
            if let Some(r) = registrar {
                register_buffr_scheme(r);
            }
        }

        fn on_before_command_line_processing(
            &self,
            _process_type: Option<&CefString>,
            command_line: Option<&mut CommandLine>,
        ) {
            let Some(command_line) = command_line else { return };
            // Defaults that make CEF behave nicely on a Linux laptop:
            //
            // - `enable-features=...` opts into Vulkan / Wayland / hardware
            //   decoding when available. CEF silently ignores features its
            //   build doesn't ship.
            // - Chromium only respects the last `enable-features` switch, so
            //   all features are merged into a single comma-separated value.
            //
            // Switches use the same names Chromium does; see chromium's
            // `chrome/common/chrome_switches.cc`.
            append_switch_with_value(
                command_line,
                "enable-features",
                // UseOzonePlatform   — Wayland/Ozone backend (Linux)
                // VaapiVideoDecodeLinuxGL — VA-API hardware video decode via GL
                // AcceleratedVideoDecodeLinuxGL — encode/decode on GPU on Linux
                // VaapiVideoEncoder  — VA-API hardware video encoding
                // CanvasOopRasterization — canvas rasterisation on the GPU
                "UseOzonePlatform,VaapiVideoDecodeLinuxGL,\
                 AcceleratedVideoDecodeLinuxGL,VaapiVideoEncoder,\
                 CanvasOopRasterization",
            );
            // GPU compositing: turn on the page compositor on the GPU even in
            // OSR mode. Without these, chrome://gpu reports "Software only"
            // for canvas, WebGL, and video decode. CEF's OSR mode does NOT
            // require software compositing — that's a historical default.
            append_switch(command_line, "enable-gpu");
            append_switch(command_line, "enable-gpu-compositing");
            append_switch(command_line, "enable-gpu-rasterization");
            append_switch(command_line, "enable-zero-copy");
            // Chromium's GPU blocklist often disables hardware accel on Linux
            // laptops with integrated GPUs. We accept the risk — modern Mesa
            // drivers handle this fine.
            append_switch(command_line, "ignore-gpu-blocklist");
            // No-sandbox is set in `Settings`, but a redundant switch
            // keeps CEF from re-enabling on certain code paths.
            append_switch(command_line, "no-sandbox");
            // Phase 6 accessibility: opt-in renderer accessibility tree.
            // The renderer feeds this into Chromium's a11y subsystem,
            // which platform screen readers consume. Off by default —
            // some sites are noticeably slower with the tree forced on.
            // Toggle via `[accessibility] force_renderer_accessibility`.
            if force_renderer_accessibility_enabled() {
                append_switch(command_line, "force-renderer-accessibility");
            }
        }

        fn browser_process_handler(&self) -> Option<BrowserProcessHandler> {
            Some(BuffrBrowserProcessHandler::new())
        }
    }
}

wrap_browser_process_handler! {
    pub struct BuffrBrowserProcessHandler;

    impl BrowserProcessHandler {
        fn on_context_initialized(&self) {
            tracing::debug!("cef: context initialized");
        }
    }
}

/// Wrap `command_line.append_switch(name)` so callers don't deal with
/// `CefString` plumbing directly.
fn append_switch(cmd: &CommandLine, name: &str) {
    let name = CefString::from(name);
    cmd.append_switch(Some(&name));
}

fn append_switch_with_value(cmd: &CommandLine, name: &str, value: &str) {
    let name = CefString::from(name);
    let value = CefString::from(value);
    cmd.append_switch_with_value(Some(&name), Some(&value));
}
