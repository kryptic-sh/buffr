//! CEF `App` impl + per-user profile path resolution.
//!
//! The `cef` crate (148.x, tauri-apps/cef-rs) exposes the `App` trait
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

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};

// `wrap_app!` / `wrap_browser_process_handler!` expand to references
// to bare `App`, `WrapApp`, `ImplApp`, `BrowserProcessHandler`, etc.
// — the upstream cefsimple example uses `use cef::*;` for this reason.
use cef::*;

use crate::view_source_scheme::register_buffr_src_scheme;

// For profile_paths()
use hjkl_config;

/// Process-wide flag toggling the `--force-renderer-accessibility`
/// switch in `on_before_command_line_processing`. Set via
/// [`set_force_renderer_accessibility`] before the first `BuffrApp` is
/// constructed (i.e. before `cef::execute_process` / `cef::initialize`).
///
/// We use a static so the wrap_app! macro's struct doesn't need to
/// carry state — the cef-rs trait surface for `App` doesn't accept
/// per-instance fields cleanly.
static FORCE_RENDERER_ACCESSIBILITY: AtomicBool = AtomicBool::new(false);
static NEXT_MESSAGE_PUMP_DELAY_MS: AtomicI64 = AtomicI64::new(-1);

/// Device scale factor (× 1000) for the `--force-device-scale-factor`
/// Chromium switch. Stored ×1000 so we can preserve fractional scales
/// (1.25, 1.5, 1.75) without floats in atomics. 0 = unset, leaving
/// CEF to its platform default. Linux has no platform autodetect, so
/// the host queries winit's primary-monitor scale and writes here
/// before `cef::initialize`. Windows + macOS use system DPI directly
/// and ignore this value.
static DEVICE_SCALE_FACTOR_X1000: AtomicU32 = AtomicU32::new(0);

/// Set the device scale factor that `on_before_command_line_processing`
/// will pass to Chromium. Caller is responsible for invoking this
/// before `cef::initialize` (and before the App is constructed for
/// `cef::execute_process` in subprocesses, though Chromium also forwards
/// the parent's switch through the helper argv automatically).
pub fn set_device_scale_factor(scale: f32) {
    let v = (scale * 1000.0).round().max(0.0) as u32;
    DEVICE_SCALE_FACTOR_X1000.store(v, Ordering::SeqCst);
}

/// Read the current device scale override. Returns `None` if unset
/// (use platform default) or `Some(scale)` for a forced value.
pub fn device_scale_factor() -> Option<f32> {
    let v = DEVICE_SCALE_FACTOR_X1000.load(Ordering::SeqCst);
    (v > 0).then(|| v as f32 / 1000.0)
}

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

pub fn take_scheduled_message_pump_delay_ms() -> Option<i64> {
    let delay = NEXT_MESSAGE_PUMP_DELAY_MS.swap(-1, Ordering::SeqCst);
    (delay >= 0).then_some(delay)
}

// ProfilePaths is defined in buffr-engine::profile and re-exported by buffr-cef.
use buffr_engine::ProfilePaths;

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
                // `buffr://` is no longer a custom CEF scheme — navigation to
                // `buffr://new` etc. is rewritten to the InternalServer HTTP
                // loopback by `BrowserHost::cef_navigation_url` before CEF sees
                // the URL. Only `buffr-src:` (view-source) still needs a custom
                // scheme registration.
                register_buffr_src_scheme(r);
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
            // Disable autofill / password-manager features that rewrite
            // input fields mid-typing. Symptom: `wtype` (or any virtual
            // keyboard) typing an email like `sitemaster16@gmail.com`
            // gets the field reverted on a subsequent keystroke after
            // autofill matches a saved partial credential, and focus
            // jumps to the password field. See buffr#36.
            append_switch_with_value(
                command_line,
                "disable-features",
                "AutofillServerCommunication,\
                 AutofillEnableAccountWalletStorage,\
                 PasswordManagerOnboarding,\
                 AutofillAddressProfileSavePromptNicknameSupport",
            );
            append_switch(command_line, "disable-save-password-bubble");
            // Force the basic (plaintext) password/cookie store. Linux
            // Chromium auto-detects org.freedesktop.Secret over D-Bus;
            // when an SS implementation is present but auth/unlock fails
            // (common with pass-secret-service, KeePassXC, etc.), Chromium
            // silently drops cookies rather than persist unencrypted.
            // Verified: stock chromium reproduces the same 0-cookie bug;
            // adding this switch restores cookie persistence.
            // Mirrored in `on_before_child_process_launch` because CEF
            // does not propagate user-added switches to subprocesses.
            append_switch_with_value(command_line, "password-store", "basic");
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
            // BUFFR_DISABLE_ZYGOTE=1 opts out of Chromium's zygote
            // pre-fork model. On headless Linux CI (pixman-sway, no
            // DBus, etc.) the zygote subprocess crashes at init with
            // "Invalid file descriptor to ICU data received" — the
            // parent's ICU fd doesn't survive the fork. Skipping the
            // zygote forks each renderer/gpu process fresh, which
            // costs a few ms at startup but sidesteps the fd issue.
            #[cfg(target_os = "linux")]
            if std::env::var_os("BUFFR_DISABLE_ZYGOTE").is_some() {
                append_switch(command_line, "no-zygote");
            }
            // macOS Chromium tries to access the user's "Chromium Safe Storage"
            // Keychain item through OSCrypt for cookie/password encryption.
            // buffr does not intentionally use that store, and prompting on
            // every dev launch is hostile, so use Chromium's mock keychain.
            // Gate to dev builds only — release builds should use the real
            // OS keychain so cookies and future saved passwords are encrypted.
            #[cfg(all(target_os = "macos", debug_assertions))]
            append_switch(command_line, "use-mock-keychain");
            // Phase 6 accessibility: opt-in renderer accessibility tree.
            // The renderer feeds this into Chromium's a11y subsystem,
            // which platform screen readers consume. Off by default —
            // some sites are noticeably slower with the tree forced on.
            // Toggle via `[accessibility] force_renderer_accessibility`.
            if force_renderer_accessibility_enabled() {
                append_switch(command_line, "force-renderer-accessibility");
            }
            // Linux HiDPI: Chromium has no platform DPI autodetect on
            // Linux (Wayland per-output scale, X11 Xft.dpi). Without
            // this switch pages render at 1× on a 2× display. The
            // host writes the winit primary-monitor scale into the
            // static before init. Windows + macOS use the OS DPI APIs
            // directly so we leave them alone.
            #[cfg(target_os = "linux")]
            if let Some(scale) = device_scale_factor()
                && (scale - 1.0).abs() > 0.01
            {
                append_switch_with_value(
                    command_line,
                    "force-device-scale-factor",
                    &format!("{scale}"),
                );
                append_switch_with_value(command_line, "high-dpi-support", "1");
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

        fn on_schedule_message_pump_work(&self, delay_ms: i64) {
            tracing::trace!(delay_ms, "cef: schedule message pump work");
            NEXT_MESSAGE_PUMP_DELAY_MS.store(delay_ms.max(0), Ordering::SeqCst);
        }

        fn on_before_child_process_launch(
            &self,
            command_line: Option<&mut CommandLine>,
        ) {
            let Some(command_line) = command_line else { return };
            // Propagate --password-store=basic to every child process
            // (renderer, GPU, utility, zygote). On Linux, Chromium's OSCrypt
            // probes for Secret Service in whichever subprocess actually
            // performs the cookie write. CEF doesn't auto-propagate this
            // switch from the browser process command line, so without this
            // hook subprocesses fall back to OSCrypt's keyring auto-detect
            // and silently drop cookies when Secret Service unlock fails.
            append_switch_with_value(command_line, "password-store", "basic");
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

/// Resolve the on-disk cache and data directories for buffr's profile.
///
/// Uses `hjkl_config::cache_dir("buffr")` and `hjkl_config::data_dir("buffr")`
/// — XDG-everywhere on Linux/macOS/Windows. Both directories are created on
/// first access by the OS; callers that need them to exist must `create_dir_all`
/// themselves.
///
/// Returns `Err(buffr_core::CoreError::NoProjectDirs)` if the platform has no
/// home dir (uncommon in practice — CI containers and servers occasionally hit this).
pub fn profile_paths() -> Result<ProfilePaths, buffr_core::CoreError> {
    // Debug builds use a separate XDG suffix so dev runs don't trample on
    // release-build profile state (cookies, sqlite, CEF cache) and vice versa.
    let app_name = if cfg!(debug_assertions) {
        "buffr-debug"
    } else {
        "buffr"
    };
    let cache =
        hjkl_config::cache_dir(app_name).map_err(|_| buffr_core::CoreError::NoProjectDirs)?;
    let data = hjkl_config::data_dir(app_name).map_err(|_| buffr_core::CoreError::NoProjectDirs)?;
    Ok(ProfilePaths { cache, data })
}
