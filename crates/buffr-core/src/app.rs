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

// `wrap_app!` / `wrap_browser_process_handler!` expand to references
// to bare `App`, `WrapApp`, `ImplApp`, `BrowserProcessHandler`, etc.
// — the upstream cefsimple example uses `use cef::*;` for this reason.
use cef::*;

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
        fn on_before_command_line_processing(
            &self,
            _process_type: Option<&CefString>,
            command_line: Option<&mut CommandLine>,
        ) {
            let Some(command_line) = command_line else { return };
            // Defaults that make CEF behave nicely on a Linux laptop:
            //
            // - `enable-features=...` opts into Vulkan / Wayland / hardware decoding
            //   when available. CEF silently ignores features its build doesn't ship.
            //
            // Switches use the same names Chromium does; see chromium's
            // `chrome/common/chrome_switches.cc`.
            append_switch_with_value(
                command_line,
                "enable-features",
                "UseOzonePlatform,VaapiVideoDecodeLinuxGL",
            );
            // No-sandbox is set in `Settings`, but a redundant switch
            // keeps CEF from re-enabling on certain code paths.
            append_switch(command_line, "no-sandbox");
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
