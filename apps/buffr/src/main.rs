//! buffr main entry point.
//!
//! Phase 1 wiring:
//!
//! 1. Init tracing.
//! 2. Dispatch to `cef::execute_process` so the same binary serves as
//!    its own renderer/GPU/utility subprocess (single-binary mode).
//! 3. Initialize CEF with [`buffr_core::BuffrApp`] + per-user paths.
//! 4. Open one winit window, hand its native handle to
//!    [`buffr_core::BrowserHost`].
//! 5. Drive winit's event loop while pumping `cef::do_message_loop_work`
//!    each iteration. (We avoid `cef::run_message_loop` so winit owns
//!    the main loop — required for native chrome in Phase 3.)
//! 6. On exit: shut CEF down cleanly.
//!
//! Phase 4 additions: clap CLI, TOML config loader, hot-reload watcher
//! that swaps the live keymap on file changes.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result};
use buffr_config::{Config, ConfigSource};
use buffr_core::{BuffrApp, init_cef_api, profile_paths};
use buffr_modal::{Engine, PageMode, Step, key_event_to_chord};
use cef::{ImplBrowser, Settings};
use clap::Parser;
use raw_window_handle::HasWindowHandle;
use tracing::{info, trace, warn};
#[cfg(all(target_os = "linux", not(feature = "osr")))]
use winit::platform::x11::EventLoopBuilderExtX11;
use winit::{
    application::ApplicationHandler,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    keyboard::ModifiersState,
    window::{Window, WindowId},
};

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Print resolved config (TOML) to stdout and exit.
    #[arg(long)]
    print_config: bool,
    /// Validate the config file and exit non-zero on failure.
    #[arg(long)]
    check_config: bool,
    /// Override config file path (default: XDG location).
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
    /// Override `general.homepage` for this run.
    #[arg(long, value_name = "URL")]
    homepage: Option<String>,
}

fn main() -> Result<()> {
    // -------- subprocess dispatch (single-binary mode) ----------------
    //
    // CEF re-launches this binary with `--type=renderer` (and other
    // worker args clap doesn't know about), so we must short-circuit
    // before parsing the user-facing CLI. `cef::execute_process`
    // returns >= 0 inside a child process and we exit with that code.
    //
    // `init_cef_api` MUST run before any other CEF call: cef-rs 147
    // wraps libcef's API-version negotiation, and skipping it triggers
    // `CefApp_0_CToCpp called with invalid version -1` the moment a
    // wrapped trait object (our `BuffrApp`) is handed to CEF.
    let is_subprocess = std::env::args().any(|a| a.starts_with("--type="));
    if is_subprocess {
        init_cef_api();
        let args = cef::args::Args::new();
        let mut app = BuffrApp::new();
        let exit_code = cef::execute_process(
            Some(args.as_main_args()),
            Some(&mut app),
            std::ptr::null_mut(),
        );
        std::process::exit(exit_code.max(0));
    }

    let cli = Cli::parse();

    // -------- short-circuit modes (no CEF init) ----------------------
    if cli.check_config {
        return run_check_config(cli.config.as_deref());
    }
    if cli.print_config {
        return run_print_config(cli.config.as_deref());
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "buffr=info,buffr_core=info".into()),
        )
        .init();

    init_cef_api();

    let args = cef::args::Args::new();
    let mut app = BuffrApp::new();

    info!("buffr v{} starting", env!("CARGO_PKG_VERSION"));
    info!("buffr-core v{}", buffr_core::version());

    // -------- profile paths --------
    let paths = profile_paths().context("resolving profile dirs")?;
    info!(cache = %paths.cache.display(), data = %paths.data.display(), "profile paths");

    // -------- load config + build initial keymap ----------------------
    let (config, source) = match buffr_config::load_and_validate(cli.config.as_deref()) {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "config load failed; falling back to defaults");
            (Config::default(), ConfigSource::Defaults)
        }
    };
    match &source {
        ConfigSource::File(p) => info!(path = %p.display(), "config loaded"),
        ConfigSource::Defaults => info!("config: built-in defaults"),
    }

    let keymap = buffr_config::build_keymap(&config).context("building keymap from config")?;
    let homepage = cli
        .homepage
        .clone()
        .unwrap_or_else(|| config.general.homepage.clone());

    // -------- CEF initialize --------
    let cache_path = paths.cache.to_string_lossy().into_owned();
    let settings = Settings {
        no_sandbox: 1,
        // Drive the loop ourselves; don't let CEF spawn its own thread.
        multi_threaded_message_loop: 0,
        // Plumb the per-user cache root so CEF doesn't fall back to its
        // process working dir (and so cookies persist across runs).
        // Field confirmed in cef-147's bindings:
        // `Settings::root_cache_path: CefString`.
        root_cache_path: cef::CefString::from(cache_path.as_str()),
        ..Default::default()
    };

    let init_ok = cef::initialize(
        Some(args.as_main_args()),
        Some(&settings),
        Some(&mut app),
        std::ptr::null_mut(),
    );
    if init_ok != 1 {
        anyhow::bail!("cef::initialize returned {init_ok} (expected 1)");
    }
    info!("cef initialized");

    // -------- winit event loop --------
    //
    // CEF's windowed embedding only supports X11 on Linux. On Wayland
    // sessions we run via XWayland — winit 0.30 prefers Wayland by
    // default when `WAYLAND_DISPLAY` is set, so we force the X11
    // backend explicitly. Native Wayland is blocked on OSR (compile
    // with `--features osr`, Phase 3 scope).
    //
    // Note: winit 0.29 removed the `WINIT_UNIX_BACKEND` env var; the
    // supported way to pin a backend in winit 0.30 is
    // `EventLoopBuilderExtX11::with_x11()` on the builder, which sets
    // `forced_backend = Backend::X` before backend selection.
    #[cfg(all(target_os = "linux", not(feature = "osr")))]
    let event_loop = {
        let session_type = std::env::var("XDG_SESSION_TYPE").unwrap_or_default();
        let wayland_display = std::env::var("WAYLAND_DISPLAY").unwrap_or_default();
        if session_type == "wayland" || !wayland_display.is_empty() {
            warn!(
                "running under XWayland — native Wayland needs OSR (Phase 3); rebuild with `--features osr` once OSR lands"
            );
        }
        EventLoop::builder()
            .with_x11()
            .build()
            .context("creating winit event loop (forced X11 backend)")?
    };

    #[cfg(not(all(target_os = "linux", not(feature = "osr"))))]
    let event_loop = EventLoop::new().context("creating winit event loop")?;

    event_loop.set_control_flow(ControlFlow::Poll);

    let engine = Arc::new(Mutex::new(Engine::new(keymap)));

    // -------- spawn config watcher (keymap-only hot reload) ------------
    //
    // Phase 4 hot-apply scope: keymap changes only. Theme / homepage
    // / startup require a restart for now — full hot-apply is Phase
    // 5+ work and needs lifecycle hooks the chrome layer doesn't have
    // yet.
    let _watcher = if let ConfigSource::File(p) = &source {
        let engine_for_watch = Arc::clone(&engine);
        match buffr_config::watch(p.clone(), move |result| match result {
            Ok(new_cfg) => match buffr_config::build_keymap(&new_cfg) {
                Ok(km) => {
                    if let Ok(mut e) = engine_for_watch.lock() {
                        e.set_keymap(km);
                        info!("config reloaded — keymap applied");
                    }
                }
                Err(err) => warn!(error = %err, "config reload: keymap rebuild failed"),
            },
            Err(err) => warn!(error = %err, "config reload failed"),
        }) {
            Ok(w) => Some(w),
            Err(err) => {
                warn!(error = %err, "could not start config watcher");
                None
            }
        }
    } else {
        None
    };

    let mut app_state = AppState::new(homepage, engine);
    if let Err(err) = event_loop.run_app(&mut app_state) {
        warn!(error = %err, "winit event loop exited with error");
    }

    // -------- shutdown --------
    info!("cef shutting down");
    cef::shutdown();
    Ok(())
}

fn run_check_config(path: Option<&std::path::Path>) -> Result<()> {
    match buffr_config::load_and_validate(path) {
        Ok((_, src)) => {
            match src {
                ConfigSource::File(p) => println!("ok: {}", p.display()),
                ConfigSource::Defaults => println!("ok: (no user config; defaults)"),
            }
            Ok(())
        }
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}

fn run_print_config(path: Option<&std::path::Path>) -> Result<()> {
    let (cfg, _) = buffr_config::load_and_validate(path).context("loading config")?;
    let s = buffr_config::to_toml_string(&cfg).context("serializing config")?;
    print!("{s}");
    Ok(())
}

/// Minimal winit `ApplicationHandler` that owns one window + one
/// CEF browser, pumping CEF's message loop on `about_to_wait`.
///
/// Phase 2 additions:
///
/// - `engine` — the modal page-mode dispatcher. Default leader is `\`
///   (vim's default).
/// - `modifiers` — winit 0.30 splits modifier state out of `KeyEvent`
///   so we track the latest `ModifiersChanged` payload here and feed
///   it alongside each pressed key.
/// - `startup` — wall-clock instant the event loop began. The engine
///   is clock-agnostic: it just needs a monotonic `Duration`. We pass
///   `startup.elapsed()` on every `feed`/`tick`.
/// - `current_mode_label` — last mode rendered into the window title;
///   only call `set_title` when this changes. winit's `set_title` is
///   idempotent but cheap → cheaper still to skip.
struct AppState {
    homepage: String,
    window: Option<Window>,
    host: Option<buffr_core::BrowserHost>,
    engine: Arc<Mutex<Engine>>,
    modifiers: ModifiersState,
    startup: Instant,
    current_mode_label: &'static str,
}

impl AppState {
    fn new(homepage: String, engine: Arc<Mutex<Engine>>) -> Self {
        Self {
            homepage,
            window: None,
            host: None,
            engine,
            modifiers: ModifiersState::empty(),
            startup: Instant::now(),
            current_mode_label: mode_label(PageMode::Normal),
        }
    }

    fn dispatch_action(&self, action: &buffr_modal::PageAction) {
        if let Some(host) = self.host.as_ref() {
            host.dispatch(action);
        } else {
            warn!(?action, "no browser host yet — dropping action");
        }
    }

    fn refresh_title(&mut self) {
        let mode = self
            .engine
            .lock()
            .map(|e| e.mode())
            .unwrap_or(PageMode::Normal);
        let label = mode_label(mode);
        if label != self.current_mode_label {
            self.current_mode_label = label;
            if let Some(window) = self.window.as_ref() {
                window.set_title(&format!("buffr — {label}"));
            }
        }
    }
}

/// Map a [`PageMode`] to the status-line label rendered into the
/// window title. `Pending` collapses to `NORMAL` because the engine
/// only enters `Pending` mid-multi-chord and we don't want the title
/// to flicker on every key.
fn mode_label(mode: PageMode) -> &'static str {
    match mode {
        PageMode::Normal | PageMode::Pending => "NORMAL",
        PageMode::Visual => "VISUAL",
        PageMode::Command => "COMMAND",
        PageMode::Hint => "HINT",
        PageMode::Edit => "EDIT",
    }
}

impl ApplicationHandler for AppState {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let win_attrs = Window::default_attributes()
            .with_title(format!("buffr — {}", self.current_mode_label))
            .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 800.0));
        let window = match event_loop.create_window(win_attrs) {
            Ok(w) => w,
            Err(err) => {
                warn!(error = %err, "failed to create window");
                event_loop.exit();
                return;
            }
        };

        let raw = match window.window_handle() {
            Ok(h) => h.as_raw(),
            Err(err) => {
                warn!(error = %err, "no raw window handle");
                event_loop.exit();
                return;
            }
        };

        match buffr_core::BrowserHost::new(raw, &self.homepage) {
            Ok(host) => {
                info!(url = %self.homepage, "browser host created");
                self.host = Some(host);
            }
            Err(err) => {
                warn!(error = %err, "failed to create browser host");
            }
        }

        self.window = Some(window);
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => {
                info!("close requested");
                event_loop.exit();
            }
            WindowEvent::RedrawRequested => {
                if let Some(window) = self.window.as_ref() {
                    window.request_redraw();
                }
            }
            WindowEvent::ModifiersChanged(mods) => {
                self.modifiers = mods.state();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let Some(chord) = key_event_to_chord(&event, self.modifiers) else {
                    return;
                };
                let now = self.startup.elapsed();
                let step = match self.engine.lock() {
                    Ok(mut e) => e.feed(chord, now),
                    Err(_) => return,
                };
                match step {
                    Step::Resolved(action) => {
                        self.dispatch_action(&action);
                    }
                    Step::Pending | Step::Ambiguous { .. } => {
                        // Phase 3 chrome will surface a count/pending
                        // buffer indicator in the status line. For
                        // now, silently accumulate.
                    }
                    Step::Reject => {
                        trace!(?chord, "key not bound");
                    }
                    Step::EditModeActive => {
                        // Edit-mode is the hjkl handoff; until that
                        // lands the chord is dropped here. The engine
                        // already updated state, so just trace.
                        trace!(?chord, "chord dropped — edit-mode integration is Phase 2b");
                    }
                }
                self.refresh_title();
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        // Pump CEF every frame. With `ControlFlow::Poll` this fires
        // continuously, which is the simplest correct cadence for
        // Phase 1 — Phase 3 will switch to a tickless wakeup.
        cef::do_message_loop_work();

        // Engine ambiguity timeout: if a single-chord prefix is
        // sitting on the buffer past the timeout window, fire the
        // shorter binding. This is the vim `&timeoutlen` behaviour.
        let now = self.startup.elapsed();
        let action = match self.engine.lock() {
            Ok(mut e) => e.tick(now),
            Err(_) => None,
        };
        if let Some(action) = action {
            self.dispatch_action(&action);
            self.refresh_title();
        }
    }
}

// Silence the "unused import" lint when no `Browser` is materialized
// yet; the trait re-export keeps method-call syntax working in `host.rs`.
#[allow(dead_code)]
fn _impl_browser_used() {
    fn _f<T: ImplBrowser>(_: &T) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_help_renders() {
        Cli::command().debug_assert();
    }
}
