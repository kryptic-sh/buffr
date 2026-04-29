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

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Minimum quiet time after the last session-dirtying event before
/// the session file is written to disk.  Sliding window: each new
/// change resets the clock.
const SESSION_SAVE_DEBOUNCE_MS: u64 = 500;

/// Grace window after a Blur event during Insert mode. If a Focus
/// event for a different field arrives within this window, the pair
/// is treated as a Tab/Shift+Tab transfer (stay in Insert, update
/// last_focused_field). Otherwise the engine flips to Normal. The
/// window absorbs the renderer→browser console-IPC latency that can
/// split a synchronous focusout/focusin pair across drain ticks.
const BLUR_TRANSFER_WINDOW_MS: u64 = 250;

/// Quiet time after the last keystroke in a `/` / `?` find overlay
/// before live-search fires `start_find` against the active tab. Each
/// keystroke resets the timer. 300 ms tracks Chromium's own find-bar
/// debounce closely enough that highlight churn doesn't lag.
const FIND_LIVE_DEBOUNCE_MS: u64 = 300;

use anyhow::{Context, Result};
use buffr_config::{ClearableData, Config, ConfigSource};
use buffr_core::cmdline::{Command, parse as parse_cmdline};
use buffr_core::{
    BuffrApp, DownloadNoticeQueue, EditConsoleEvent, EditEventSink, FindResultSink, HintAction,
    HintAlphabet, HintEventSink, PermissionsQueue, PopupCloseSink, PopupCreateSink, PromptOutcome,
    SharedOsrFrame, SharedOsrViewState, TabId, drain_edit_events, drain_permissions_with_defer,
    drain_popup_closes, drain_popup_creates, drain_popup_urls, expire_stale_notices, init_cef_api,
    new_download_notice_queue, new_edit_event_sink, new_find_sink, new_hint_event_sink,
    new_permissions_queue, peek_download_notice, peek_permission_front, permissions_queue_len,
    pop_permission_front, profile_paths, register_buffr_handler_factory,
};
use buffr_modal::{
    Engine, EngineModifiers, Key, NamedKey, PageMode, PlannedInput, SpecialKey, Step,
    key_event_to_chord, key_event_to_chord_with_repeat,
};
use buffr_permissions::Permissions;
use buffr_ui::{
    CertState, DOWNLOAD_NOTICE_HEIGHT, DownloadNoticeKind, DownloadNoticeStrip, FindStatus,
    HintStatus as UiHintStatus, InputBar, PermissionsPrompt, STATUSLINE_HEIGHT, Statusline,
    Suggestion, SuggestionKind, TAB_STRIP_HEIGHT, TabStrip, TabView,
};

mod render;
mod session;
use cef::{ImplBrowser, KeyEvent, KeyEventType, MouseButtonType, Settings};
use clap::Parser;
use raw_window_handle::HasWindowHandle;
use tempfile::TempDir;
use tracing::{debug, info, trace, warn};
use winit::{
    application::ApplicationHandler,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy},
    keyboard::ModifiersState,
    window::{Window, WindowId},
};

/// Custom user events sent into the winit loop from background threads.
#[derive(Debug, Clone)]
enum BuffrUserEvent {
    /// CEF OSR on_paint fired for the main browser; request main-window redraw.
    OsrFrame,
    /// CEF OSR on_paint fired for popup browser `browser_id`; request that
    /// popup's window redraw.
    OsrFramePopup(i32),
}

/// Per-popup-window state. Owns the winit window, wgpu renderer, and the
/// OSR frame/view shared with the CEF paint handler.
struct PopupWindow {
    window: Arc<Window>,
    renderer: crate::render::Renderer,
    /// CEF browser id — used to route CEF close events back to this window.
    browser_id: i32,
    frame: SharedOsrFrame,
    #[allow(dead_code)]
    view: SharedOsrViewState,
    /// URL shown in the popup's address bar. Updated by CEF `on_address_change`.
    url: String,
    /// Generation of the last OSR frame we composited.
    last_osr_generation: u64,
    /// Reusable scratch buffer for the same mem::swap trick as the main window.
    osr_scratch: Vec<u8>,
    /// Chrome generation counter — bumped when URL or size changes.
    chrome_generation: u64,
    /// Chrome generation at the last GPU upload.
    last_painted_chrome_gen: u64,
    /// Last cursor position in window coordinates (adjusted for address bar).
    cursor: (i32, i32),
    /// CEF bitmask of mouse buttons currently held.
    mouse_buttons: u32,
    /// Winit modifier state for this popup's events.
    modifiers: ModifiersState,
    /// Click state for double-click detection.
    last_click_at: Instant,
    last_click_button: Option<cef::MouseButtonType>,
    click_count: i32,
}

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
    /// Import bookmarks from a Netscape Bookmark File (HTML). Runs
    /// without launching CEF; prints the import count to stdout.
    #[arg(long, value_name = "PATH")]
    import_bookmarks: Option<PathBuf>,
    /// Print every bookmark to stdout and exit. Debug aid until UI lands.
    #[arg(long)]
    list_bookmarks: bool,
    /// Print every bookmark tag (sorted) to stdout and exit.
    #[arg(long)]
    list_bookmarks_tags: bool,
    /// Print every download (most-recent first) to stdout and exit.
    /// Debug aid until the downloads pane lands (Phase 5b chrome).
    #[arg(long)]
    list_downloads: bool,
    /// Delete every `Completed` download row (keeps Failed/Canceled).
    /// Prints the count removed.
    #[arg(long)]
    clear_completed_downloads: bool,
    /// Print every persisted zoom override (`<domain>\t<level>`) and
    /// exit. Debug aid until UI lands.
    #[arg(long)]
    list_zoom: bool,
    /// Wipe the per-site zoom store. Prints the count of rows removed.
    #[arg(long)]
    clear_zoom: bool,
    /// Run in private mode: every store is in-memory, the CEF cache
    /// lives in a tempdir under `$TMPDIR/buffr-private-<pid>` that is
    /// deleted on shutdown. Nothing persists across restarts.
    ///
    /// This is single-window incognito — there is no IPC isolation
    /// from other buffr processes; full-process compartmentalisation
    /// (Tor-Browser-grade) is out of scope for Phase 5.
    #[arg(long)]
    private: bool,
    /// Smoke-test flag for Phase 3 find-in-page wiring. After the
    /// browser is created and the homepage starts loading, kicks off
    /// a single find for `<query>` (forward search). Match counts
    /// are routed through the statusline; tracing also logs each
    /// `OnFindResult` tick so the smoke job can scrape them.
    #[arg(long, value_name = "QUERY")]
    find: Option<String>,
    /// Open this URL in an extra tab on launch. Repeatable; tabs are
    /// added in order after any restored session and the homepage.
    #[arg(long = "new-tab", value_name = "URL", action = clap::ArgAction::Append)]
    new_tab: Vec<String>,
    /// Skip session restore for this run. The homepage opens in a
    /// single tab and a fresh session file is written on exit.
    #[arg(long)]
    no_restore: bool,
    /// Print the saved session (one URL per line, `*` prefix for
    /// pinned tabs) to stdout and exit. Does not launch CEF.
    #[arg(long)]
    list_session: bool,
    /// Print every persisted permission decision and exit.
    /// Output: `<origin>\t<capability>\t<decision>\t<set_at>`.
    #[arg(long)]
    list_permissions: bool,
    /// Wipe the permissions table. Prints the count of rows removed.
    #[arg(long)]
    clear_permissions: bool,
    /// Drop every stored permission decision for `<ORIGIN>`. Prints
    /// the count of rows removed.
    #[arg(long, value_name = "ORIGIN")]
    forget_origin: Option<String>,
    /// Print every history entry (most-recent first) and exit.
    /// Debug aid until the history UI lands. See also `--history-limit`.
    #[arg(long)]
    list_history: bool,
    /// Frecency-search history for `<QUERY>` and print matches, best
    /// first. Mutually exclusive with `--list-history` (search wins).
    #[arg(long, value_name = "QUERY")]
    search_history: Option<String>,
    /// Maximum rows returned by `--list-history` / `--search-history`.
    /// Defaults to 100.
    #[arg(long, value_name = "N")]
    history_limit: Option<usize>,
    /// Print the telemetry on/off state, the on-disk counter file
    /// path, and the current counter table; exit 0. No CEF init.
    #[arg(long)]
    telemetry_status: bool,
    /// Reset every counter to zero (truncates the on-disk JSON to
    /// `{}`). No-op when telemetry is disabled. Prints "telemetry
    /// counters reset" and exits 0.
    #[arg(long)]
    reset_telemetry: bool,
    /// Print every captured panic report (most recent first) and
    /// exit 0. No CEF init.
    #[arg(long)]
    list_crashes: bool,
    /// Delete crash reports older than `crash_reporter.purge_after_days`.
    /// Prints "purged N reports" and exits 0.
    #[arg(long)]
    purge_crashes: bool,
    /// Phase 6 update channel: hit GitHub releases now, print the
    /// resolved status, exit 0. No CEF init. Honors
    /// `[updates] enabled = false` (prints `disabled` without any
    /// network call).
    #[arg(long)]
    check_for_updates: bool,
    /// Read the on-disk update cache and print the cached status. No
    /// network. No CEF init. The statusline reads the same cache.
    #[arg(long)]
    update_status: bool,
    /// Print every default-bound `PageAction` and the keys that bind
    /// it. Exits 0 — used to verify keyboard-only paths for the a11y
    /// audit. No CEF init.
    #[arg(long)]
    audit_keymap: bool,
    /// Force the X11 backend on Linux. No effect on macOS / Windows.
    /// Useful for testing the X11 path on a Wayland session.
    #[cfg(target_os = "linux")]
    #[arg(long)]
    x11: bool,
}

fn main() -> Result<()> {
    // -------- macOS framework loader ---------------------------------
    //
    // On macOS the libcef framework is bundled inside the .app and
    // must be loaded explicitly through cef-rs's `LibraryLoader`
    // before any CEF entry. This applies equally to the browser
    // process and the subprocess case: both run from the same binary
    // in single-binary mode, but in macOS bundles the helper is a
    // separate executable that loads the framework with `helper=true`
    // (path-resolved via `../../..` instead of `../Frameworks`).
    #[cfg(target_os = "macos")]
    {
        let exe = std::env::current_exe().context("resolving current_exe for LibraryLoader")?;
        let loader = cef::library_loader::LibraryLoader::new(&exe, false);
        if !loader.load() {
            anyhow::bail!("failed to load CEF framework via LibraryLoader");
        }
        // Keep the loader alive for the lifetime of the process —
        // `Drop` calls `unload_library`, which we only want at exit.
        std::mem::forget(loader);
    }

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
    if let Some(path) = cli.import_bookmarks.as_deref() {
        return run_import_bookmarks(path);
    }
    if cli.list_bookmarks {
        return run_list_bookmarks();
    }
    if cli.list_bookmarks_tags {
        return run_list_bookmarks_tags();
    }
    if cli.list_downloads {
        return run_list_downloads();
    }
    if cli.clear_completed_downloads {
        return run_clear_completed_downloads();
    }
    if cli.list_zoom {
        return run_list_zoom();
    }
    if cli.clear_zoom {
        return run_clear_zoom();
    }
    if cli.list_session {
        return run_list_session();
    }
    if cli.list_permissions {
        return run_list_permissions();
    }
    if cli.clear_permissions {
        return run_clear_permissions();
    }
    if let Some(origin) = cli.forget_origin.as_deref() {
        return run_forget_origin(origin);
    }
    if cli.telemetry_status {
        return run_telemetry_status(cli.config.as_deref());
    }
    if cli.reset_telemetry {
        return run_reset_telemetry(cli.config.as_deref());
    }
    if cli.list_crashes {
        return run_list_crashes();
    }
    if cli.purge_crashes {
        return run_purge_crashes(cli.config.as_deref());
    }
    if cli.check_for_updates {
        return run_check_for_updates(cli.config.as_deref());
    }
    if cli.update_status {
        return run_update_status(cli.config.as_deref());
    }
    if cli.audit_keymap {
        return run_audit_keymap();
    }
    if cli.search_history.is_some() || cli.list_history {
        let limit = cli.history_limit.unwrap_or(100);
        return run_query_history(cli.search_history.as_deref(), limit);
    }

    // Debug builds default to DEBUG, release builds to INFO. Both
    // honor RUST_LOG when set explicitly.
    let default_filter = if cfg!(debug_assertions) {
        "buffr=debug,buffr_core=debug"
    } else {
        "buffr=info,buffr_core=info"
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| default_filter.into()),
        )
        .init();

    init_cef_api();

    let args = cef::args::Args::new();
    let mut app = BuffrApp::new();

    info!("buffr v{} starting", env!("CARGO_PKG_VERSION"));
    info!("buffr-core v{}", buffr_core::version());

    // -------- profile paths (persistent) or tempdir (--private) ------
    //
    // Private mode replaces both `cache` and `data` with a freshly-
    // created `TempDir` under `$TMPDIR/buffr-private-<pid>`. The dir
    // is deleted by `Drop` at process exit. Stores are opened
    // in-memory, so no SQLite file ever appears on disk.
    let (paths, _private_tmp) = resolve_paths(cli.private)?;
    if cli.private {
        info!("private mode active — no data persists across restart");
        debug!(
            cache = %paths.cache.display(),
            data = %paths.data.display(),
            "private mode paths"
        );
    } else {
        info!("profile paths resolved");
        debug!(cache = %paths.cache.display(), data = %paths.data.display(), "profile paths");
    }

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

    // -------- history store --------
    //
    // Phase 5: SQLite-backed history at
    // `<data>/history.sqlite`. `BrowserHost` keeps an `Arc<History>`
    // and CEF's `LoadHandler` / `DisplayHandler` (wired in
    // `buffr_core::handlers`) pump every main-frame visit + title
    // into it. Private mode opens an in-memory DB instead.
    let history = Arc::new(if cli.private {
        buffr_history::History::open_in_memory_with_skip_schemes(
            config.privacy.skip_schemes.clone(),
        )
        .context("opening in-memory history")?
    } else {
        buffr_history::History::open_with_skip_schemes(
            paths.data.join("history.sqlite"),
            config.privacy.skip_schemes.clone(),
        )
        .context("opening history database")?
    });
    let initial_rows = history.count().unwrap_or(0);
    info!(rows = initial_rows, "history opened");

    // -------- bookmarks store --------
    //
    // Phase 5: SQLite-backed bookmarks at
    // `<data>/bookmarks.sqlite`. Constructed but no auto-callback —
    // bookmarks are user-action-driven (Phase 5 UI work). We hand the
    // `Arc<Bookmarks>` to `AppState` so the future omnibar / chrome
    // already has a handle to query.
    let bookmarks = Arc::new(if cli.private {
        buffr_bookmarks::Bookmarks::open_in_memory().context("opening in-memory bookmarks")?
    } else {
        buffr_bookmarks::Bookmarks::open(paths.data.join("bookmarks.sqlite"))
            .context("opening bookmarks database")?
    });
    let initial_bookmarks = bookmarks.count().unwrap_or(0);
    info!(rows = initial_bookmarks, "bookmarks opened");

    // -------- zoom store --------
    //
    // Phase 5: SQLite-backed per-site zoom levels at
    // `<data>/zoom.sqlite`. `BrowserHost` writes through on
    // ZoomIn/Out/Reset; the CEF `LoadHandler` reads on each
    // `on_load_end` to restore the level for the loaded domain.
    let zoom = Arc::new(if cli.private {
        buffr_zoom::ZoomStore::open_in_memory().context("opening in-memory zoom store")?
    } else {
        buffr_zoom::ZoomStore::open(paths.data.join("zoom.sqlite")).context("opening zoom store")?
    });

    // -------- permissions store --------
    //
    // Phase 5: SQLite-backed per-origin permission decisions at
    // `<data>/permissions.sqlite`. The CEF `PermissionHandler`
    // pre-checks remembered decisions; any uncached request enqueues
    // onto the shared `PermissionsQueue` for the UI thread to prompt.
    let permissions = Arc::new(if cli.private {
        Permissions::open_in_memory().context("opening in-memory permissions")?
    } else {
        Permissions::open(paths.data.join("permissions.sqlite"))
            .context("opening permissions database")?
    });
    let permissions_queue = new_permissions_queue();

    // -------- downloads store + resolved config -----------------------
    //
    // Resolve `default_dir` once at startup so the CEF download
    // handler doesn't have to re-resolve on every event. We also
    // create the directory if it's missing so the very first download
    // doesn't fail with ENOENT before CEF gets a chance to fall back.
    let downloads = Arc::new(if cli.private {
        buffr_downloads::Downloads::open_in_memory().context("opening in-memory downloads")?
    } else {
        buffr_downloads::Downloads::open(paths.data.join("downloads.sqlite"))
            .context("opening downloads database")?
    });
    let initial_downloads = downloads.count().unwrap_or(0);
    info!(rows = initial_downloads, "downloads opened");

    let mut downloads_config = config.downloads.clone();
    if downloads_config.default_dir.is_none() {
        downloads_config.default_dir = Some(buffr_config::resolve_default_dir(&downloads_config));
    }
    if let Some(dir) = downloads_config.default_dir.as_ref() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            warn!(path = %dir.display(), error = %e, "downloads default_dir mkdir failed");
        }
        debug!(path = %dir.display(), "downloads default_dir resolved");
    }
    let downloads_config = Arc::new(downloads_config);

    let keymap = buffr_config::build_keymap(&config).context("building keymap from config")?;
    let homepage = cli
        .homepage
        .clone()
        .unwrap_or_else(|| config.general.homepage.clone());

    // -------- telemetry counters --------
    //
    // Phase 6: opt-in usage counters. When `[privacy] enable_telemetry`
    // is `false` (the default) every method is a no-op and no file is
    // written. When `true`, increments accumulate in memory and flush
    // on shutdown plus once a minute via the background task.
    //
    // Private mode disables telemetry unconditionally — the whole
    // point of `--private` is "leave no traces". The counter store is
    // still constructed (so call sites don't have to branch) but the
    // `enabled` flag is forced off.
    let telemetry_enabled = config.privacy.enable_telemetry && !cli.private;
    let counters_path = if cli.private {
        // Private mode tempdir; nothing persists past Drop.
        paths.data.join("usage-counters.json")
    } else {
        paths.data.join("usage-counters.json")
    };
    let counters = Arc::new(buffr_core::UsageCounters::open(
        &counters_path,
        telemetry_enabled,
    ));
    if telemetry_enabled {
        info!(path = %counters_path.display(), "telemetry counters enabled");
    } else {
        tracing::debug!("telemetry: disabled (no-op)");
    }

    // -------- crash reporter --------
    //
    // Phase 6: opt-in panic-hook reporter. Writes JSON crash files
    // under `<data>/crashes/`. Disabled-default; the install call is
    // a no-op when the config flag is false.
    let crash_dir = paths.data.join("crashes");
    if config.crash_reporter.enabled && !cli.private {
        buffr_core::CrashReporter::install(crash_dir.clone(), true);
    }

    // -------- accessibility flag --------
    //
    // Phase 6: when `[accessibility] force_renderer_accessibility = true`,
    // CEF's `App::on_before_command_line_processing` injects the
    // `--force-renderer-accessibility` switch so the renderer feeds an
    // accessibility tree to platform screen readers. Default off
    // because the tree adds non-trivial per-frame work.
    //
    // Toggling AFTER `BuffrApp::new()` is too late on the renderer
    // side — the helper subprocess re-reads `force_renderer_accessibility_enabled`
    // when it runs `BuffrApp::new()` itself, so we keep the toggle
    // sticky across processes. (Helper doesn't share memory; it
    // re-evaluates the env. We currently don't propagate this flag to
    // helpers via env — TODO Phase 6b.)
    buffr_core::set_force_renderer_accessibility(config.accessibility.force_renderer_accessibility);

    // -------- update channel --------
    //
    // Phase 6 update channel: cache lives at `<data>/update-cache.json`.
    // The statusline reads `check_cached()` once at startup so the
    // indicator surfaces without a live network call. Users run
    // `buffr --check-for-updates` to refresh.
    let update_cache_path = paths.data.join("update-cache.json");
    let update_checker = Arc::new(buffr_core::UpdateChecker::new(
        config.updates.clone(),
        update_cache_path,
    ));
    let initial_update_status = update_checker.check_cached();

    // -------- CEF initialize --------
    let cache_path = paths.cache.to_string_lossy().into_owned();
    let mut settings = Settings {
        no_sandbox: 1,
        // Drive the loop ourselves; don't let CEF spawn its own thread.
        multi_threaded_message_loop: 0,
        // Plumb the per-user cache root so CEF doesn't fall back to its
        // process working dir (and so cookies persist across runs).
        // Field confirmed in cef-147's bindings:
        // `Settings::root_cache_path: CefString`.
        root_cache_path: cef::CefString::from(cache_path.as_str()),
        // Must be set at init time for OSR to be usable. Has no effect
        // on windowed browsers; safe to always enable.
        windowless_rendering_enabled: 1,
        ..Default::default()
    };
    configure_macos_dev_cef_settings(&mut settings)?;

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

    // Phase 6 telemetry: count the successful CEF init as one
    // `app_starts` event. No-op when disabled. We tick *after*
    // `cef::initialize` returns 1 so a launch that crashes during CEF
    // boot doesn't get counted as a successful start.
    counters.increment(buffr_core::KEY_APP_STARTS);

    // -------- winit event loop --------
    //
    // Allow winit to pick the best Wayland backend. Linux always uses
    // HostMode::Osr (wgpu composite over Wayland/macOS) — X11/XWayland
    // windowed embedding is not supported, and AppKit child views do not
    // layer cleanly with buffr's custom chrome. Windows uses native
    // child-window embedding (HostMode::Windowed).
    //
    // On Linux, `--x11` forces the X11 backend even on a Wayland session
    // (useful for regression testing the X11 path).
    let event_loop = {
        let mut builder = EventLoop::<BuffrUserEvent>::with_user_event();
        #[cfg(target_os = "linux")]
        {
            use winit::platform::x11::EventLoopBuilderExtX11;
            if cli.x11 {
                builder.with_x11();
            }
            // Default: winit auto-picks Wayland when WAYLAND_DISPLAY is
            // set, otherwise X11. No explicit call needed.
        }
        builder.build().context("creating winit event loop")?
    };

    event_loop.set_control_flow(ControlFlow::Poll);

    let engine = Arc::new(Mutex::new(Engine::new(keymap)));

    // Register the `buffr://` scheme handler factory after the engine
    // exists so the new-tab renderer can read the live keymap on each
    // request (hot-reloaded user overrides land on the next visit).
    {
        let engine_for_newtab = Arc::clone(&engine);
        let provider: buffr_core::NewTabHtmlProvider =
            Arc::new(move || render_new_tab_html(&engine_for_newtab));
        register_buffr_handler_factory(provider);
    }

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

    let find_sink = new_find_sink();
    let hint_sink = new_hint_event_sink();
    // Edit-mode: construct the event queue so it can be threaded through
    // AppState → BrowserHost → handlers. Drained each tick; keys forward
    // directly to CEF once a field is focused (no Rust EditSession).
    let edit_sink = new_edit_event_sink();
    // Build the hint alphabet up front so a misconfigured config
    // surfaces an error before CEF has a chance to start. The
    // validator already checked the same invariants but `from_str` is
    // the type-safe constructor, so we run it again here.
    let hint_alphabet = HintAlphabet::from_str(&config.hint.alphabet).unwrap_or_else(|err| {
        warn!(error = %err, "hint alphabet rejected — falling back to default");
        HintAlphabet::from_str(buffr_core::DEFAULT_HINT_ALPHABET)
            .expect("default alphabet always valid")
    });

    let search_config = Arc::new(config.search.clone());

    // -------- session restore -----------------------------------------
    //
    // Read the saved tab list (skipped under `--private` / `--no-restore`).
    // The first entry, if any, supersedes the homepage as the
    // initial-tab URL; remaining entries open in the background once
    // the window exists. CLI `--new-tab` URLs append after that.
    let session_path = if cli.private {
        None
    } else {
        Some(session::default_path(&paths.data))
    };
    let (pending_session_tabs, pending_session_active): (Vec<(String, bool)>, Option<usize>) =
        if cli.private || cli.no_restore {
            (Vec::new(), None)
        } else if let Some(p) = session_path.as_ref() {
            match session::read(p) {
                Ok(Some(s)) => {
                    info!(
                        path = %p.display(),
                        pinned = s.pinned.len(),
                        tabs = s.tabs.len(),
                        active = ?s.active,
                        "session: restored",
                    );
                    let entries: Vec<(String, bool)> =
                        s.entries().map(|(u, p)| (u.to_string(), p)).collect();
                    (entries, s.active)
                }
                Ok(None) => (Vec::new(), None),
                Err(err) => {
                    warn!(error = %err, "session: read failed — starting fresh");
                    (Vec::new(), None)
                }
            }
        } else {
            (Vec::new(), None)
        };

    let download_notice_queue = new_download_notice_queue();

    let shutdown_flag = Arc::new(AtomicBool::new(false));
    {
        let flag = Arc::clone(&shutdown_flag);
        if let Err(err) = ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst)) {
            warn!(error = %err, "ctrlc handler already installed — using existing");
        }
    }
    let mut app_state = AppState::new(
        homepage,
        engine,
        history.clone(),
        bookmarks.clone(),
        downloads.clone(),
        downloads_config,
        zoom.clone(),
        permissions.clone(),
        permissions_queue.clone(),
        download_notice_queue,
        search_config,
        cli.private,
        find_sink,
        hint_sink,
        edit_sink,
        hint_alphabet,
        cli.find.clone(),
        cli.new_tab.clone(),
        pending_session_tabs,
        pending_session_active,
        session_path,
        counters.clone(),
        update_checker.clone(),
        initial_update_status,
        config.theme.high_contrast,
        shutdown_flag,
        event_loop.create_proxy(),
    );
    if let Err(err) = event_loop.run_app(&mut app_state) {
        warn!(error = %err, "winit event loop exited with error");
    }

    // Shutdown sequence — order is critical. CEF browsers must close
    // and fully release before `cef::shutdown()`, and all CEF refs we
    // hold must drop while CEF's threads are still alive. Mishandling
    // any step segfaults during the GPU process teardown on builds
    // with hardware compositing.
    info!("shutdown: closing browsers");
    if let Some(host) = app_state.host.as_ref() {
        host.close_all_browsers();
    }

    // Drop ONLY the host first. This releases every Browser ref while
    // CEF's threads are still running, so CEF can finish the close
    // callbacks instead of segfaulting on dangling refs during its
    // own shutdown.
    info!("shutdown: dropping host");
    drop(app_state.host.take());

    // Drop the wgpu renderer BEFORE cef::shutdown(). Both touch the
    // same EGL / GL / Vulkan driver state on Linux; tearing down
    // wgpu after CEF has dismantled the GPU process segfaults.
    info!("shutdown: dropping renderer");
    drop(app_state.renderer.take());
    // Drop all popup renderers for the same reason.
    info!("shutdown: dropping popup renderers");
    app_state.popups.clear();

    // Defer-dismiss any permission requests still queued at shutdown.
    // Dropping a CEF callback without invoking it would wedge the
    // renderer; resolving with `Defer` fires the right "DISMISS"
    // outcome on each.
    drain_permissions_with_defer(&permissions_queue, &permissions);

    // -------- clear-on-exit --------
    //
    // Honour `[privacy] clear_on_exit` before tearing CEF down so
    // cookie deletion routes through a still-live `CookieManager`.
    // Private mode skips this entirely — the tempdir's `Drop` removes
    // everything anyway.
    if !cli.private {
        run_clear_on_exit(
            &config.privacy.clear_on_exit,
            &paths,
            &history,
            &bookmarks,
            &downloads,
        );
    }

    // -------- telemetry flush --------
    //
    // Final flush before CEF shutdown. No-op when telemetry is
    // disabled. Errors log at WARN inside `flush()` and never
    // propagate — telemetry must not block exit.
    counters.flush();

    // -------- shutdown --------
    info!("shutdown: cef shutting down");
    cef::shutdown();
    info!("shutdown: cef::shutdown returned");
    // Drop the rest of AppState now (renderer/wgpu, window, engine,
    // sinks). CEF is fully gone, so wgpu can release the GPU surface
    // without racing CEF's GPU process teardown.
    info!("shutdown: dropping app_state remainder");
    drop(app_state);
    info!("shutdown: app_state dropped");
    // Tempdir drops here (after CEF is gone), removing the private
    // profile root tree.
    drop(_private_tmp);
    info!("shutdown: complete");
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

/// Open the bookmarks store at the standard data path. Used by the
/// CLI short-circuits below (no CEF init needed).
fn open_bookmarks_for_cli() -> Result<buffr_bookmarks::Bookmarks> {
    let paths = profile_paths().context("resolving profile dirs")?;
    std::fs::create_dir_all(&paths.data).context("creating data dir")?;
    let bm = buffr_bookmarks::Bookmarks::open(paths.data.join("bookmarks.sqlite"))
        .context("opening bookmarks database")?;
    Ok(bm)
}

fn run_import_bookmarks(path: &std::path::Path) -> Result<()> {
    let html =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let bm = open_bookmarks_for_cli()?;
    let n = bm.import_netscape(&html).context("importing bookmarks")?;
    println!("imported {n} bookmarks");
    Ok(())
}

fn run_list_bookmarks() -> Result<()> {
    let bm = open_bookmarks_for_cli()?;
    let all = bm.all().context("loading bookmarks")?;
    for b in &all {
        let title = b.title.as_deref().unwrap_or("");
        let tags = if b.tags.is_empty() {
            String::new()
        } else {
            format!(" [{}]", b.tags.join(","))
        };
        println!("{}\t{}\t{}{}", b.id.0, b.url, title, tags);
    }
    Ok(())
}

fn run_list_bookmarks_tags() -> Result<()> {
    let bm = open_bookmarks_for_cli()?;
    for tag in bm.all_tags().context("loading tags")? {
        println!("{tag}");
    }
    Ok(())
}

/// Open the downloads store at the standard data path. Used by the
/// CLI short-circuits — no CEF init.
fn open_downloads_for_cli() -> Result<buffr_downloads::Downloads> {
    let paths = profile_paths().context("resolving profile dirs")?;
    std::fs::create_dir_all(&paths.data).context("creating data dir")?;
    let dl = buffr_downloads::Downloads::open(paths.data.join("downloads.sqlite"))
        .context("opening downloads database")?;
    Ok(dl)
}

fn run_list_downloads() -> Result<()> {
    let dl = open_downloads_for_cli()?;
    let all = dl.all(1024).context("loading downloads")?;
    for d in &all {
        let status = match d.status {
            buffr_downloads::DownloadStatus::InFlight => "in_flight",
            buffr_downloads::DownloadStatus::Completed => "completed",
            buffr_downloads::DownloadStatus::Canceled => "canceled",
            buffr_downloads::DownloadStatus::Failed => "failed",
        };
        let path = d.full_path.as_deref().unwrap_or("-");
        let total = d
            .total_bytes
            .map(|n| n.to_string())
            .unwrap_or_else(|| "?".into());
        println!(
            "{}\t{}\t{}\t{}/{}\t{}\t{}",
            d.id.0, status, d.url, d.received_bytes, total, d.suggested_name, path
        );
    }
    Ok(())
}

fn run_clear_completed_downloads() -> Result<()> {
    let dl = open_downloads_for_cli()?;
    let n = dl
        .clear_completed()
        .context("clearing completed downloads")?;
    println!("cleared {n} completed downloads");
    Ok(())
}

/// Open the zoom store at the standard data path. Used by the CLI
/// short-circuits — no CEF init.
fn open_zoom_for_cli() -> Result<buffr_zoom::ZoomStore> {
    let paths = profile_paths().context("resolving profile dirs")?;
    std::fs::create_dir_all(&paths.data).context("creating data dir")?;
    let z = buffr_zoom::ZoomStore::open(paths.data.join("zoom.sqlite"))
        .context("opening zoom database")?;
    Ok(z)
}

fn run_list_zoom() -> Result<()> {
    let z = open_zoom_for_cli()?;
    for (domain, level) in z.all().context("loading zoom rows")? {
        println!("{domain}\t{level}");
    }
    Ok(())
}

fn run_clear_zoom() -> Result<()> {
    let z = open_zoom_for_cli()?;
    let n = z.clear().context("clearing zoom rows")?;
    println!("cleared {n} zoom rows");
    Ok(())
}

/// Open the history store at the standard data path. Used by the CLI
/// short-circuits — no CEF init. Skip-schemes only matter for recording,
/// not for reading, so we pass the canonical defaults.
fn open_history_for_cli() -> Result<buffr_history::History> {
    let paths = profile_paths().context("resolving profile dirs")?;
    std::fs::create_dir_all(&paths.data).context("creating data dir")?;
    let h = buffr_history::History::open(paths.data.join("history.sqlite"))
        .context("opening history database")?;
    Ok(h)
}

/// `--list-history` / `--search-history` short-circuit.
///
/// When `search` is `Some`, performs a frecency search; otherwise lists
/// the `limit` most-recent visits. Output: one row per visit,
/// tab-separated: `<id>\t<visit_time RFC3339>\t<transition>\t<url>\t<title-or-empty>`.
fn run_query_history(search: Option<&str>, limit: usize) -> Result<()> {
    let h = open_history_for_cli()?;
    let entries = match search {
        Some(q) => h.search(q, limit).context("searching history")?,
        None => h.recent(limit).context("loading recent history")?,
    };
    for e in &entries {
        let title = e.title.as_deref().unwrap_or("");
        println!(
            "{}\t{}\t{}\t{}\t{}",
            e.id,
            e.visit_time.to_rfc3339(),
            e.transition.as_str(),
            e.url,
            title
        );
    }
    Ok(())
}

/// Open the permissions store at the standard data path. Used by the
/// CLI short-circuits — no CEF init.
fn open_permissions_for_cli() -> Result<Permissions> {
    let paths = profile_paths().context("resolving profile dirs")?;
    std::fs::create_dir_all(&paths.data).context("creating data dir")?;
    let p = Permissions::open(paths.data.join("permissions.sqlite"))
        .context("opening permissions database")?;
    Ok(p)
}

fn run_list_permissions() -> Result<()> {
    let p = open_permissions_for_cli()?;
    for row in p.all().context("loading permissions")? {
        let dec = match row.decision {
            buffr_permissions::Decision::Allow => "allow",
            buffr_permissions::Decision::Deny => "deny",
        };
        println!(
            "{}\t{}\t{}\t{}",
            row.origin,
            row.capability.as_storage_key(),
            dec,
            row.set_at
        );
    }
    Ok(())
}

fn run_clear_permissions() -> Result<()> {
    let p = open_permissions_for_cli()?;
    let n = p.clear().context("clearing permissions")?;
    println!("cleared {n} permission rows");
    Ok(())
}

fn run_forget_origin(origin: &str) -> Result<()> {
    let p = open_permissions_for_cli()?;
    let n = p
        .forget_origin(origin)
        .context("forgetting permissions for origin")?;
    println!("forgot {n} permission rows for {origin}");
    Ok(())
}

/// Path the [`buffr_core::UsageCounters`] writes to. Stable across
/// callers — `--telemetry-status` and the live runtime resolve here.
fn telemetry_path() -> Result<PathBuf> {
    let paths = profile_paths().context("resolving profile dirs")?;
    std::fs::create_dir_all(&paths.data).context("creating data dir")?;
    Ok(paths.data.join("usage-counters.json"))
}

/// Crash report directory. Created lazily on first install.
fn crash_dir() -> Result<PathBuf> {
    let paths = profile_paths().context("resolving profile dirs")?;
    Ok(paths.data.join("crashes"))
}

fn load_config_or_default(path: Option<&std::path::Path>) -> Config {
    match buffr_config::load_and_validate(path) {
        Ok((cfg, _)) => cfg,
        Err(_) => Config::default(),
    }
}

fn run_telemetry_status(config_path: Option<&std::path::Path>) -> Result<()> {
    let cfg = load_config_or_default(config_path);
    let path = telemetry_path()?;
    let enabled = cfg.privacy.enable_telemetry;
    let counters = buffr_core::UsageCounters::open(&path, enabled);
    let label = if enabled { "enabled" } else { "disabled" };
    println!("telemetry: {} (path: {})", label, path.display());
    let snapshot = counters.read().context("reading telemetry counters")?;
    if snapshot.is_empty() {
        println!("(no counters recorded)");
    } else {
        // Sorted output so the line ordering is deterministic.
        let mut keys: Vec<&String> = snapshot.keys().collect();
        keys.sort();
        for k in keys {
            println!("{}\t{}", k, snapshot[k]);
        }
    }
    Ok(())
}

fn run_reset_telemetry(config_path: Option<&std::path::Path>) -> Result<()> {
    let cfg = load_config_or_default(config_path);
    let path = telemetry_path()?;
    let counters = buffr_core::UsageCounters::open(&path, cfg.privacy.enable_telemetry);
    counters.reset().context("resetting telemetry counters")?;
    println!("telemetry counters reset");
    Ok(())
}

fn run_list_crashes() -> Result<()> {
    let dir = crash_dir()?;
    let crashes = buffr_core::CrashReporter::list_crashes(&dir);
    if crashes.is_empty() {
        println!("(no crash reports at {})", dir.display());
        return Ok(());
    }
    for c in &crashes {
        let location = c.location.as_deref().unwrap_or("-");
        println!(
            "{}\t{}\t{}\t{}",
            c.timestamp.to_rfc3339(),
            c.buffr_version,
            location,
            c.message
        );
    }
    Ok(())
}

fn run_purge_crashes(config_path: Option<&std::path::Path>) -> Result<()> {
    let cfg = load_config_or_default(config_path);
    let dir = crash_dir()?;
    let n = buffr_core::CrashReporter::purge_older_than(&dir, cfg.crash_reporter.purge_after_days)
        .context("purging crash reports")?;
    println!("purged {n} reports");
    Ok(())
}

/// Resolve the update-cache path. Stable across the live runtime and
/// the `--check-for-updates` / `--update-status` short-circuits.
fn update_cache_path() -> Result<PathBuf> {
    let paths = profile_paths().context("resolving profile dirs")?;
    std::fs::create_dir_all(&paths.data).context("creating data dir")?;
    Ok(paths.data.join("update-cache.json"))
}

fn print_update_status(status: &buffr_core::UpdateStatus) {
    use buffr_core::UpdateStatus as U;
    match status {
        U::Disabled => println!("disabled"),
        U::UpToDate { current } => println!("up-to-date\t{current}"),
        U::Available { current, latest } => {
            println!(
                "available\t{}\t{}\t{}\t{}",
                current, latest.version, latest.tag, latest.url
            );
        }
        U::Stale {
            last_checked,
            latest,
        } => {
            println!(
                "stale\t{}\t{}\t{}\t{}",
                last_checked.to_rfc3339(),
                latest.version,
                latest.tag,
                latest.url
            );
        }
        U::NetworkError(msg) => println!("error\t{msg}"),
    }
}

/// Project [`buffr_core::UpdateStatus`] onto the
/// [`buffr_ui::UpdateIndicator`] surface. `Available` and `Stale`
/// surface; everything else hides.
/// Render the `buffr://new` page bytes — substitutes the keymap into
/// the embedded template each time the page is requested so config
/// hot-reloads land without a binary rebuild.
fn render_new_tab_html(engine: &Arc<Mutex<Engine>>) -> Vec<u8> {
    use std::collections::BTreeMap;
    // Group chord-strings by action so multiple binds for the same
    // action collapse onto one row.
    let mut grouped: BTreeMap<String, Vec<String>> = BTreeMap::new();
    if let Ok(e) = engine.lock() {
        let entries = e.keymap().entries(buffr_modal::PageMode::Normal);
        for (chords, action) in entries {
            let keys: String = chords.iter().map(|c| c.to_string()).collect();
            grouped.entry(format!("{action:?}")).or_default().push(keys);
        }
    }
    for keys in grouped.values_mut() {
        keys.sort();
    }
    let body = if grouped.is_empty() {
        "<tr><td class=\"empty\" colspan=\"2\">no bindings</td></tr>".to_string()
    } else {
        let mut s = String::with_capacity(grouped.len() * 96);
        for (action, keys) in &grouped {
            use std::fmt::Write;
            let kbds: String = keys
                .iter()
                .map(|k| format!("<kbd>{}</kbd>", html_escape(k)))
                .collect::<Vec<_>>()
                .join(" ");
            let _ = write!(
                s,
                "<tr><td class=\"k\">{}</td><td class=\"a\">{}</td></tr>",
                kbds,
                html_escape(action),
            );
        }
        s
    };
    buffr_core::NEW_TAB_HTML_TEMPLATE
        .replacen(buffr_core::NEW_TAB_KEYBINDS_MARKER, &body, 1)
        .into_bytes()
}

/// Minimal HTML escaper for the new-tab page renderer. Covers the
/// five characters that matter when injecting keybinding labels into
/// table cells.
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

fn update_indicator_from(status: &buffr_core::UpdateStatus) -> Option<buffr_ui::UpdateIndicator> {
    match status {
        buffr_core::UpdateStatus::Available { .. } => Some(buffr_ui::UpdateIndicator::Available),
        buffr_core::UpdateStatus::Stale { .. } => Some(buffr_ui::UpdateIndicator::Stale),
        _ => None,
    }
}

fn run_check_for_updates(config_path: Option<&std::path::Path>) -> Result<()> {
    let cfg = load_config_or_default(config_path);
    let path = update_cache_path()?;
    let checker = buffr_core::UpdateChecker::new(cfg.updates.clone(), path);
    let status = checker.check_now();
    print_update_status(&status);
    Ok(())
}

fn run_update_status(config_path: Option<&std::path::Path>) -> Result<()> {
    let cfg = load_config_or_default(config_path);
    let path = update_cache_path()?;
    let checker = buffr_core::UpdateChecker::new(cfg.updates.clone(), path);
    let status = checker.check_cached();
    print_update_status(&status);
    Ok(())
}

/// `--audit-keymap` — print every default-bound `PageAction` plus the
/// chord(s) that bind it. Format: `<mode>\t<keys>\t<action>`. Sorted by
/// mode then keys for stable output. Used to verify keyboard-only
/// reachability (Phase 6 a11y).
fn run_audit_keymap() -> Result<()> {
    let rows = buffr_modal::Keymap::audit_default_bindings('\\');
    for (mode, keys, action) in &rows {
        println!("{mode}\t{keys}\t{action:?}");
    }
    Ok(())
}

/// `--list-session` short-circuit. Prints one row per saved tab to
/// stdout: `*\t<url>` when pinned, `\t<url>` otherwise. Schema
/// version is printed on stderr for diagnostic clarity.
fn run_list_session() -> Result<()> {
    let paths = profile_paths().context("resolving profile dirs")?;
    let path = session::default_path(&paths.data);
    match session::read(&path)? {
        None => {
            eprintln!("no saved session at {}", path.display());
        }
        Some(s) => {
            eprintln!("schema version: {}", s.version);
            for (url, pinned) in s.entries() {
                let pin = if pinned { "*" } else { " " };
                println!("{pin}\t{url}");
            }
        }
    }
    Ok(())
}

/// Resolve the (cache, data) profile paths. Returns the resolved
/// [`buffr_core::ProfilePaths`] plus an optional [`TempDir`] that owns
/// the lifetime of the `--private` tree (so the caller can drop it
/// after CEF shuts down).
///
/// Persistent layout: standard XDG via `directories::ProjectDirs`.
///
/// Private layout: `$TMPDIR/buffr-private-<pid>/{cache,data}`. The
/// `<pid>` suffix means concurrent private launches each get their
/// own root (no clobbering); the inner `cache` and `data` split
/// matches the persistent shape so the rest of the codebase doesn't
/// need conditionals.
fn resolve_paths(private: bool) -> Result<(buffr_core::ProfilePaths, Option<TempDir>)> {
    if private {
        let pid = std::process::id();
        let prefix = format!("buffr-private-{pid}-");
        let tmp = tempfile::Builder::new()
            .prefix(&prefix)
            .tempdir()
            .context("creating private-mode tempdir")?;
        let cache = tmp.path().join("cache");
        let data = tmp.path().join("data");
        std::fs::create_dir_all(&cache).context("creating private cache subdir")?;
        std::fs::create_dir_all(&data).context("creating private data subdir")?;
        Ok((buffr_core::ProfilePaths { cache, data }, Some(tmp)))
    } else {
        let paths = profile_paths().context("resolving profile dirs")?;
        std::fs::create_dir_all(&paths.cache).context("creating profile cache dir")?;
        std::fs::create_dir_all(&paths.data).context("creating profile data dir")?;
        Ok((paths, None))
    }
}

#[cfg(target_os = "macos")]
fn configure_macos_dev_cef_settings(settings: &mut Settings) -> Result<()> {
    // Let CEF tell us when the browser process needs work. Blindly calling
    // CefDoMessageLoopWork from every winit callback can re-enter AppKit
    // while winit is already handling an event.
    settings.external_message_pump = 1;

    let exe = std::env::current_exe().context("resolving current_exe for macOS CEF settings")?;
    if exe.components().any(|c| c.as_os_str() == "Contents") {
        return Ok(());
    }

    let exe_dir = exe
        .parent()
        .context("current_exe has no parent for macOS CEF settings")?;
    let framework_dir = exe_dir
        .join("../Frameworks/Chromium Embedded Framework.framework")
        .canonicalize()
        .context("resolving staged CEF framework for cargo run")?;
    let resources_dir = framework_dir.join("Resources");

    settings.browser_subprocess_path = cef::CefString::from(exe.to_string_lossy().as_ref());
    settings.framework_dir_path = cef::CefString::from(framework_dir.to_string_lossy().as_ref());
    settings.resources_dir_path = cef::CefString::from(resources_dir.to_string_lossy().as_ref());
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn configure_macos_dev_cef_settings(_settings: &mut Settings) -> Result<()> {
    Ok(())
}

/// Honour `[privacy] clear_on_exit` after the event loop returns and
/// before `cef::shutdown()`. Each entry is processed independently —
/// one failure doesn't skip the rest. Errors log at WARN; successes
/// log at INFO so the user can see what was wiped.
///
/// Cookies + LocalStorage path: cookies route through CEF's
/// global cookie manager (`cef::cookie_manager_get_global_manager`);
/// localStorage is a tree under `<root_cache_path>/Local Storage` that
/// we delete directly because CEF doesn't expose a programmatic flush
/// for it. Cache is similarly a directory delete. History / Bookmarks
/// / Downloads route through the corresponding store's `clear_all`.
fn run_clear_on_exit(
    items: &[ClearableData],
    paths: &buffr_core::ProfilePaths,
    history: &buffr_history::History,
    bookmarks: &buffr_bookmarks::Bookmarks,
    downloads: &buffr_downloads::Downloads,
) {
    if items.is_empty() {
        return;
    }
    info!(count = items.len(), "clear_on_exit: running");
    // Dedupe so repeats in config don't cause double work.
    let mut seen = std::collections::HashSet::new();
    for &item in items {
        if !seen.insert(item) {
            continue;
        }
        match item {
            ClearableData::Cookies => clear_cookies(),
            ClearableData::Cache => clear_dir(&paths.cache.join("Cache"), "cache"),
            ClearableData::History => match history.clear_all() {
                Ok(n) => info!(rows = n, "clear_on_exit: history cleared"),
                Err(err) => warn!(error = %err, "clear_on_exit: history failed"),
            },
            ClearableData::Bookmarks => match bookmarks.clear_all() {
                Ok(n) => info!(rows = n, "clear_on_exit: bookmarks cleared"),
                Err(err) => warn!(error = %err, "clear_on_exit: bookmarks failed"),
            },
            ClearableData::Downloads => match downloads.clear_all() {
                Ok(n) => info!(rows = n, "clear_on_exit: downloads cleared"),
                Err(err) => warn!(error = %err, "clear_on_exit: downloads failed"),
            },
            ClearableData::LocalStorage => {
                clear_dir(&paths.cache.join("Local Storage"), "local_storage")
            }
        }
    }
}

/// Best-effort delete of a CEF-managed directory tree. CEF recreates
/// the dir on next startup. ENOENT is silently swallowed.
fn clear_dir(path: &std::path::Path, label: &str) {
    if !path.exists() {
        info!(path = %path.display(), label, "clear_on_exit: dir absent — skipping");
        return;
    }
    match std::fs::remove_dir_all(path) {
        Ok(()) => info!(path = %path.display(), label, "clear_on_exit: dir wiped"),
        Err(err) => {
            warn!(path = %path.display(), label, error = %err, "clear_on_exit: dir wipe failed")
        }
    }
}

/// Wipe every cookie via CEF's global cookie manager. cef-147's
/// `CookieManager::delete_cookies(None, None, None)` returns 1 on
/// successful submission, 0 on synchronous failure, and dispatches
/// the actual deletion asynchronously on the IO thread. We don't pass
/// a `DeleteCookiesCallback` — the wipe runs to completion when CEF's
/// IO thread is shut down by `cef::shutdown()` immediately after.
///
/// The flush_store hop afterward forces any in-memory cookie state
/// to be persisted before we tear down — relevant for cookies that
/// arrived just before the user closed the window.
fn clear_cookies() {
    let Some(manager) = cef::cookie_manager_get_global_manager(None) else {
        warn!("clear_on_exit: cookie_manager_get_global_manager returned None");
        return;
    };
    use cef::ImplCookieManager;
    let submitted = manager.delete_cookies(None, None, None);
    if submitted == 0 {
        warn!("clear_on_exit: delete_cookies returned 0 (synchronous failure)");
    } else {
        info!("clear_on_exit: cookies — delete dispatched");
    }
    let _ = manager.flush_store(None);
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
    /// URL loaded into a fresh tab everywhere — cold-start tab 0,
    /// `:tabnew`, the `gh` chord, and `o`/`O`. Defaults to
    /// `buffr://new` and is overridable via `general.homepage` and
    /// `--homepage`.
    homepage: String,
    // Drop order matters at shutdown: `host` MUST drop before `window`
    // and `renderer`. CEF browsers hold raw handles tied to the window
    // surface and to the GPU process; dropping the window or wgpu
    // device first leaves CEF dereferencing freed memory during its
    // own teardown. Rust drops struct fields in declaration order, so
    // host comes first.
    host: Option<buffr_core::BrowserHost>,
    window: Option<Arc<Window>>,
    engine: Arc<Mutex<Engine>>,
    history: Arc<buffr_history::History>,
    bookmarks: Arc<buffr_bookmarks::Bookmarks>,
    downloads: Arc<buffr_downloads::Downloads>,
    downloads_config: Arc<buffr_config::DownloadsConfig>,
    zoom: Arc<buffr_zoom::ZoomStore>,
    permissions: Arc<Permissions>,
    permissions_queue: PermissionsQueue,
    /// Active permission prompt (if any). `Some` while the front of
    /// `permissions_queue` is being shown. Keystrokes route to the
    /// prompt resolution path while this is set.
    permissions_prompt: Option<PermissionsPrompt>,
    /// Pending close-pinned-tab confirmation. When `Some(id)`, a
    /// yes/no banner is shown and the close is gated on the user's
    /// answer (`y` / yes-button → close; `n` / no-button / `<Esc>`
    /// → dismiss). Mutually exclusive with `permissions_prompt` for
    /// rendering — the confirmation wins the slot.
    confirm_close_pinned: Option<buffr_core::TabId>,
    /// Passive download-notification queue. CEF's `DownloadHandler`
    /// pushes notices onto this; the render loop composites the front
    /// notice (if any) above the permissions strip. Notices self-expire
    /// via [`expire_stale_notices`] on each `about_to_wait` tick.
    ///
    /// Layout (top → bottom when both are active):
    ///   1. Input bar (overlay, when open)
    ///   2. Download notice strip (28 px, when a notice is queued)
    ///   3. Permissions prompt (60 px, when active)
    ///   4. Tab strip (always)
    ///   5. CEF page area
    ///   6. Statusline (always)
    download_notice_queue: DownloadNoticeQueue,
    /// Resolved search config used by the omnibar's URL-or-search
    /// resolver on Enter.
    search_config: Arc<buffr_config::Search>,
    /// Active overlay (top-of-window input bar). `None` when the
    /// engine is in any non-overlay mode; the CEF child rect uses the
    /// full vertical space minus the bottom statusline.
    overlay: Option<OverlayState>,
    /// Whether the runtime is in `--private` mode. Drives the title
    /// stamp and is purely informational — the storage layer already
    /// captured the choice at construction time.
    private: bool,
    modifiers: ModifiersState,
    startup: Instant,
    current_mode_label: &'static str,
    /// Last full window title we set. Cached so we only call winit's
    /// `set_title` when mode or URL actually changes.
    current_title: String,
    /// Find-in-page mailbox shared with the CEF `FindHandler`. The UI
    /// thread polls this each frame and copies the latest result
    /// into `statusline.find_query`.
    find_sink: FindResultSink,
    /// Hint-mode mailbox shared with the CEF display handler.
    /// `BrowserHost::pump_hint_events` drains it each tick.
    hint_sink: HintEventSink,
    /// Edit-mode event queue shared with the CEF load handler (which
    /// injects `edit.js`) and display handler (which parses its console
    /// output). Drained each `about_to_wait` tick to update focus state.
    edit_sink: EditEventSink,
    /// Current edit-mode focus state. Drives keyboard routing.
    /// Updated via [`drain_edit_events`] each tick and by the Esc path.
    edit_focus: EditFocus,
    /// Wall-clock instant of the most recent user gesture that should
    /// allow auto-entering Insert mode on the next page-driven focusin
    /// (left mouse press, `i` chord). When unset or stale, focusin
    /// events from the page are ignored — pages can't drag us into
    /// Insert via autofocus or programmatic `.focus()` calls.
    insert_intent_at: Option<Instant>,
    /// Wall-clock of the most recent in-Insert-mode Blur. The mode
    /// flip to Normal is deferred by `BLUR_TRANSFER_WINDOW` so a
    /// Tab/Shift+Tab navigation between fields (which fires
    /// blur(old) → focus(new)) is treated as a transfer rather than
    /// an exit. Cleared when a transferring Focus arrives or when the
    /// window expires.
    pending_blur_at: Option<Instant>,
    /// Index of the tab the user pressed left-click on inside the tab
    /// strip. Set on press, cleared on release; if the release lands on
    /// a different tab slot, the drag triggers a `move_tab`.
    tab_drag_src: Option<usize>,
    /// When the omnibar was auto-opened by `o` / `O` (TabNewRight /
    /// TabNewLeft), this carries the freshly-created tab's id. If the
    /// user cancels the omnibar without confirming a URL, the tab is
    /// closed (unless it's the only remaining tab).
    cancel_closes_tab: Option<buffr_core::TabId>,
    /// Debounced live-search trigger for the find overlay. Each
    /// keystroke while a `/` or `?` overlay is open pushes this
    /// `FIND_LIVE_DEBOUNCE_MS` into the future; `about_to_wait` fires
    /// `start_find` once the deadline elapses with no further input.
    /// `None` outside Find overlays or after the latest tick fired.
    find_live_due: Option<Instant>,
    /// Set whenever something the session-restore cares about changes
    /// (tab open / close / reorder / active switch / URL navigation).
    /// `about_to_wait` flushes the session JSON to disk while this is
    /// true, then clears it. On shutdown we only re-save when dirty.
    session_dirty: bool,
    /// Timestamp of the most recent event that set `session_dirty`.
    /// The actual write is deferred until `SESSION_SAVE_DEBOUNCE_MS`
    /// has elapsed since this instant (sliding window — each new dirty
    /// event resets the clock). `None` when the session is clean.
    session_dirty_since: Option<Instant>,
    /// Snapshot of the active tab's URL at the last session save.
    /// Compared against `host.active_tab_live_url()` each tick to
    /// detect navigation.
    last_session_url: String,
    /// Snapshot of `host.active_index()` at the last session save.
    last_session_active: Option<usize>,
    /// Snapshot of the tab count + ID list at the last session save —
    /// detects open / close / reorder events the moment they happen.
    last_session_tab_ids: Vec<buffr_core::TabId>,
    /// Wall-clock instant of the last `active_tab_live_url()` call.
    /// Throttled to ~4 Hz (250 ms) to bound the cef-rs
    /// "Invalid UTF-16 string" stderr spam during page loads.
    last_url_poll: Instant,
    /// Cross-thread wake handle for the winit event loop. Cloned and
    /// installed into `BrowserHost` so OSR `on_paint` from the CEF IO
    /// thread can post a redraw without polling.
    event_proxy: EventLoopProxy<BuffrUserEvent>,
    /// Configured hint alphabet, threaded through to the host on
    /// browser creation.
    hint_alphabet: HintAlphabet,
    /// One-shot smoke query for `--find`. Drained once the browser
    /// has loaded enough that `start_find` is meaningful (see the
    /// `find_smoke_at` deadline below).
    pending_find: Option<String>,
    /// Wall-clock deadline at which `pending_find` is dispatched.
    /// CEF refuses `find` until at least one frame has been laid out;
    /// 1.5 s is a comfortable margin without a real load-finished
    /// signal (Phase 3b will tie this to `OnLoadEnd`).
    find_smoke_at: Option<Instant>,
    /// Latest statusline render input. Mutated on mode change, find
    /// tick, count buffer change; the `RedrawRequested` handler
    /// repaints from this without re-deriving from the engine.
    statusline: Statusline,
    /// Tab strip render input. Refreshed from the host's tab list on
    /// every `about_to_wait` tick so the chrome reflects open / close
    /// / switch transitions without a manual signal.
    tab_strip: TabStrip,
    /// Pre-built list of URLs to open as extra tabs after the
    /// homepage / restored session has loaded. Drained by
    /// [`AppState::open_pending_tabs`] once the window exists.
    pending_new_tabs: Vec<String>,
    /// Restored session snapshot (URL + pinned bit). The first tab in
    /// the list becomes the active tab on startup; subsequent entries
    /// open in the background.
    pending_session_tabs: Vec<(String, bool)>,
    /// Path the runtime persists the live tab list to on clean
    /// shutdown. `None` in private mode (sessions never persist).
    session_path: Option<PathBuf>,
    /// wgpu-based present layer. Initialised in `resumed`; replaces the
    /// former softbuffer context + surface pair.
    renderer: Option<crate::render::Renderer>,
    /// Last cursor-blink toggle timestamp. We flip
    /// `overlay.input.cursor_visible` every 500ms while an overlay is
    /// open. Static frame (no widget redraw cost when the overlay is
    /// closed).
    cursor_blink_at: Instant,
    /// Phase 6 usage counters. Threaded through to `BrowserHost` for
    /// `tabs_opened` / `pages_loaded` / `downloads_completed`; used
    /// directly here for `bookmarks_added` / `searches_run`.
    counters: Arc<buffr_core::UsageCounters>,
    /// Last counter-flush timestamp. Background flush runs every
    /// 60 s (telemetry is low-volume; the ~1 KB JSON write is cheap
    /// but pointless to do per-tick).
    counters_flush_at: Instant,
    /// Phase 6 update channel: shared checker for the live runtime.
    /// Currently the statusline reads `check_cached()` once at startup;
    /// background re-checks would land here when scheduled. Held so
    /// the cache lifetime tracks the AppState's even though the
    /// runtime doesn't currently call `check_now` from the UI thread.
    #[allow(dead_code)]
    update_checker: Arc<buffr_core::UpdateChecker>,
    /// OSR composite: generation token of the last frame we blitted.
    /// When the CEF paint handler bumps `OsrFrame::generation` past this
    /// we know there is new content to show; when they match we can skip
    /// the BGRA→RGB copy and re-present the existing buffer.
    last_osr_generation: u64,
    /// Last known cursor position in browser-region coordinates.
    /// Updated on every `CursorMoved` event; used when forwarding click and
    /// wheel events so we don't have to thread the position through each arm.
    osr_cursor: (i32, i32),
    /// Timestamp of the last mouse click, used for double-click detection.
    osr_last_click_at: Instant,
    /// Button of the last click.  `None` before the first click.
    osr_last_click_button: Option<cef::MouseButtonType>,
    /// Click count within the current double-click window (1 or 2).
    osr_click_count: i32,
    /// Cursor position when the left mouse button was last pressed.
    /// `None` between drags. On left-button release, if the cursor
    /// has moved more than `DRAG_THRESHOLD_PX` from this position the
    /// engine transitions to Visual mode so the user can `y` the
    /// selection. CEF natively renders the on-screen text selection
    /// during the drag.
    osr_drag_start: Option<(i32, i32)>,
    /// CEF event-flag bitmask of mouse buttons currently held. OR'd
    /// into the `modifiers` field of `MouseEvent` on every `CursorMoved`
    /// so Chromium knows the left button is down during a drag and
    /// extends the text selection. Bits: 16 = left, 32 = middle,
    /// 64 = right (CEF `EVENTFLAG_*_MOUSE_BUTTON`). Set on press,
    /// cleared on release.
    osr_mouse_buttons: u32,
    /// Wheel-momentum state. Native Chrome decelerates after a touchpad
    /// flick via the gesture-recognizer / smooth-scroll path; CEF's
    /// `send_mouse_wheel_event` API is event-driven only, so we synthesize
    /// the deceleration in `about_to_wait` after high-res input goes
    /// quiet. `osr_wheel_velocity` tracks the most recent CEF-unit delta;
    /// `osr_wheel_last_at` is the last time we forwarded a real wheel
    /// event. Cleared when momentum drops below the cutoff.
    osr_wheel_velocity: (f32, f32),
    osr_wheel_last_at: Option<Instant>,
    /// Two-finger horizontal-swipe back/forward gesture state. Only
    /// `PixelDelta` events accumulate (touchpad). A gesture is bounded
    /// by `SWIPE_GAP_MS` of inactivity. Once the accumulated horizontal
    /// distance crosses `SWIPE_THRESHOLD_PX` while staying horizontal-
    /// dominant, we fire HistoryBack/Forward once and `swipe_committed`
    /// suppresses further nav until the gesture restarts.
    swipe_accum_x: f32,
    swipe_accum_y: f32,
    swipe_last_at: Option<Instant>,
    swipe_committed: bool,
    /// Ctrl+C handler flag. Set to `true` by the `ctrlc` handler;
    /// polled in `about_to_wait` to exit with a single key press.
    shutdown_flag: Arc<AtomicBool>,
    /// Next time CEF expects a pump, or `None` when idle.
    /// Set by `OnScheduleMessagePumpWork(delay_ms)`; cleared after
    /// pumping so we wait for CEF to schedule the next work item.
    cef_next_pump_at: Option<Instant>,
    /// Ordered list of `TabId`s mirroring `tab_strip.tabs`. Refreshed
    /// every `about_to_wait` tick alongside the strip; used for
    /// tab-strip click hit-testing.
    tab_ids: Vec<TabId>,
    /// Active-tab index read from the restored session. Applied once in
    /// [`AppState::open_pending_tabs`] after all session tabs are opened,
    /// then cleared so subsequent ticks don't re-apply it.
    pending_session_active: Option<usize>,
    /// The buffr-assigned field ID of the most recently focused input on
    /// the current page load. Used by `FocusFirstInput` (`i`) to restore
    /// focus to the last-touched field rather than always jumping to the
    /// first one. Reset to `None` on navigation (IDs are per-load).
    last_focused_field: Option<String>,
    /// Monotonic counter bumped on every chrome state change (mode, URL,
    /// tabs, overlay, popups, download notices, window resize). The chrome
    /// texture is only re-uploaded when this differs from
    /// `last_painted_chrome_gen`.
    chrome_generation: u64,
    /// Value of `chrome_generation` at the last chrome texture upload.
    /// When equal to `chrome_generation`, the texture is valid and no
    /// repaint is needed.
    last_painted_chrome_gen: u64,
    /// Reusable scratch buffer swapped with the OSR frame's pixel Vec on
    /// each paint. Avoids cloning ~W×H×4 bytes inside the SharedOsrFrame
    /// mutex — `mem::swap` is a few-ns pointer move, so the lock is held
    /// only long enough to grab the latest pixels and release CEF's
    /// `on_paint` thread to fill the next buffer. Reused across frames so
    /// no per-paint allocation; CEF's on_paint resizes the empty buffer
    /// it gets back exactly once after the swap.
    osr_scratch: Vec<u8>,
    /// Live popup windows keyed by their winit `WindowId`.
    popups: HashMap<WindowId, PopupWindow>,
    /// Reverse map: CEF browser id → winit `WindowId`, for fast lookup
    /// in the PopupCloseSink drain and CEF event routing.
    popup_window_id_by_browser: HashMap<i32, WindowId>,
    /// Popup-created event queue. Drained each `about_to_wait` tick to
    /// spawn new popup windows. Obtained from `host.popup_create_sink()`.
    popup_create_sink: PopupCreateSink,
    /// Popup-closed event queue. Drained each `about_to_wait` tick to
    /// drop popup windows. Obtained from `host.popup_close_sink()`.
    popup_close_sink: PopupCloseSink,
}

/// Edit-mode focus state machine.
///
/// Transitions:
///   `None` → (JS focusin event) → `Editing`
///   `Editing` → (Esc) → `None`
///   `Editing` → (JS Blur event for same field) → `None`
enum EditFocus {
    /// No editable field is focused.
    None,
    /// JS reported a focused field; keys forward directly to CEF.
    Editing { field_id: String },
}

/// Active overlay above the CEF page area.
///
/// All variants wrap the same [`InputBar`]; the discriminant decides
/// which suggestion source to query and how to handle Enter. The
/// engine sits in [`PageMode::Command`] for all, so the discriminant
/// is the only way to tell them apart at dispatch time.
#[derive(Debug)]
enum OverlayState {
    Command(InputBar),
    Omnibar(InputBar),
    Find { forward: bool, bar: InputBar },
}

impl OverlayState {
    fn input(&self) -> &InputBar {
        match self {
            OverlayState::Command(b) | OverlayState::Omnibar(b) => b,
            OverlayState::Find { bar, .. } => bar,
        }
    }
    fn input_mut(&mut self) -> &mut InputBar {
        match self {
            OverlayState::Command(b) | OverlayState::Omnibar(b) => b,
            OverlayState::Find { bar, .. } => bar,
        }
    }
}

impl AppState {
    #[allow(clippy::too_many_arguments)]
    fn new(
        homepage: String,
        engine: Arc<Mutex<Engine>>,
        history: Arc<buffr_history::History>,
        bookmarks: Arc<buffr_bookmarks::Bookmarks>,
        downloads: Arc<buffr_downloads::Downloads>,
        downloads_config: Arc<buffr_config::DownloadsConfig>,
        zoom: Arc<buffr_zoom::ZoomStore>,
        permissions: Arc<Permissions>,
        permissions_queue: PermissionsQueue,
        download_notice_queue: DownloadNoticeQueue,
        search_config: Arc<buffr_config::Search>,
        private: bool,
        find_sink: FindResultSink,
        hint_sink: HintEventSink,
        edit_sink: EditEventSink,
        hint_alphabet: HintAlphabet,
        pending_find: Option<String>,
        pending_new_tabs: Vec<String>,
        pending_session_tabs: Vec<(String, bool)>,
        pending_session_active: Option<usize>,
        session_path: Option<PathBuf>,
        counters: Arc<buffr_core::UsageCounters>,
        update_checker: Arc<buffr_core::UpdateChecker>,
        initial_update_status: buffr_core::UpdateStatus,
        high_contrast: bool,
        shutdown_flag: Arc<AtomicBool>,
        event_proxy: EventLoopProxy<BuffrUserEvent>,
    ) -> Self {
        let update_indicator = update_indicator_from(&initial_update_status);
        let mut statusline = Statusline {
            url: homepage.clone(),
            private,
            cert_state: CertState::Unknown,
            update_indicator,
            high_contrast,
            ..Statusline::default()
        };
        statusline.mode = PageMode::Normal;
        Self {
            homepage,
            host: None,
            window: None,
            engine,
            history,
            bookmarks,
            downloads,
            downloads_config,
            zoom,
            permissions,
            permissions_queue,
            permissions_prompt: None,
            confirm_close_pinned: None,
            download_notice_queue,
            search_config,
            overlay: None,
            private,
            modifiers: ModifiersState::empty(),
            startup: Instant::now(),
            current_mode_label: mode_label(PageMode::Normal),
            current_title: String::new(),
            find_sink,
            hint_sink,
            edit_sink,
            edit_focus: EditFocus::None,
            insert_intent_at: None,
            pending_blur_at: None,
            tab_drag_src: None,
            cancel_closes_tab: None,
            find_live_due: None,
            hint_alphabet,
            pending_find,
            find_smoke_at: None,
            statusline,
            tab_strip: TabStrip::default(),
            pending_new_tabs,
            pending_session_tabs,
            pending_session_active,
            session_path,
            renderer: None,
            cursor_blink_at: Instant::now(),
            counters,
            counters_flush_at: Instant::now(),
            update_checker,
            last_osr_generation: 0,
            osr_cursor: (0, 0),
            osr_last_click_at: Instant::now(),
            osr_last_click_button: None,
            osr_click_count: 1,
            osr_drag_start: None,
            osr_mouse_buttons: 0,
            osr_wheel_velocity: (0.0, 0.0),
            osr_wheel_last_at: None,
            swipe_accum_x: 0.0,
            swipe_accum_y: 0.0,
            swipe_last_at: None,
            swipe_committed: false,
            shutdown_flag,
            cef_next_pump_at: None,
            tab_ids: Vec::new(),
            session_dirty: false,
            session_dirty_since: None,
            last_session_url: String::new(),
            last_focused_field: None,
            last_session_active: None,
            last_session_tab_ids: Vec::new(),
            last_url_poll: Instant::now(),
            event_proxy,
            chrome_generation: 1,
            last_painted_chrome_gen: 0,
            osr_scratch: Vec::new(),
            popups: HashMap::new(),
            popup_window_id_by_browser: HashMap::new(),
            // Replaced in `resumed` once the host is constructed.
            popup_create_sink: buffr_core::new_popup_create_sink(),
            popup_close_sink: buffr_core::new_popup_close_sink(),
        }
    }

    /// Window title. Persistent runs render `buffr — NORMAL — <url>`;
    /// private mode inserts a marker between the brand and the mode
    /// stamp so glancing at the taskbar makes the privacy state
    /// obvious: `buffr — PRIVATE — NORMAL — <url>`. The URL trailer is
    /// omitted when no page is loaded yet.
    fn title_for(&self, mode_label: &str, url: &str) -> String {
        let head = if self.private {
            format!("buffr — PRIVATE — {mode_label}")
        } else {
            format!("buffr — {mode_label}")
        };
        if url.is_empty() {
            head
        } else {
            format!("{head} — {url}")
        }
    }

    /// Mark the chrome texture as needing a repaint.
    fn mark_chrome_dirty(&mut self) {
        self.chrome_generation = self.chrome_generation.wrapping_add(1);
    }

    fn dispatch_action(&mut self, action: &buffr_modal::PageAction) {
        use buffr_modal::PageAction as A;
        // Adjacent-tab opens require both a host call and a &mut self call
        // (open_omnibar). Handle them before the shared host borrow so the
        // borrow checker sees two disjoint borrows.
        if matches!(action, A::TabNewRight | A::TabNewLeft) {
            let Some(host) = self.host.as_ref() else {
                warn!(?action, "no browser host yet — dropping action");
                return;
            };
            let raw_idx = if matches!(action, A::TabNewRight) {
                host.active_index().unwrap_or(0).saturating_add(1)
            } else {
                host.active_index().unwrap_or(0)
            };
            // The new tab is unpinned, so clamp to the unpinned region
            // (i.e. at or after the last pinned slot). Otherwise an
            // `O` from the first pinned tab would push the unpinned
            // entry into the pinned-only leading band.
            let insert_idx = raw_idx.max(host.pinned_count());
            // Last use of `host` — NLL releases the shared borrow here.
            let result = host.open_tab_at(&self.homepage, insert_idx);
            match result {
                Ok(new_id) => {
                    // If the user cancels the omnibar without typing a
                    // URL, this tab gets closed back out (unless it's
                    // the last tab open).
                    self.cancel_closes_tab = Some(new_id);
                    self.open_omnibar();
                }
                Err(ref err) => warn!(error = %err, "tab_new adjacent: failed"),
            }
            return;
        }

        let Some(host) = self.host.as_ref() else {
            warn!(?action, "no browser host yet — dropping action");
            return;
        };
        // Tab actions need apps-layer policy decisions (e.g. last-tab
        // close → exit) so they bypass the host dispatcher's fallback
        // path.
        match action {
            A::TabNewRight | A::TabNewLeft => unreachable!("handled above"),
            A::TabNew => {
                let url = self.homepage.clone();
                if let Err(err) = host.open_tab(&url) {
                    warn!(error = %err, %url, "tab_new: failed");
                }
            }
            A::TabClose => {
                self.close_active_tab_or_exit();
            }
            A::TabNext => host.next_tab(),
            A::TabPrev => host.prev_tab(),
            A::PinTab => {
                host.toggle_pin_active();
                self.refresh_tab_strip();
                self.mark_session_dirty();
                self.request_redraw();
            }
            A::PasteUrl { after } => {
                let active_idx = host.active_index().unwrap_or(0);
                let insert_idx = if *after {
                    active_idx.saturating_add(1)
                } else {
                    active_idx
                };
                let url = match host.clipboard_text() {
                    Some(t) => t,
                    None => return,
                };
                let trimmed = url.trim();
                if trimmed.is_empty() {
                    return;
                }
                if !matches!(
                    buffr_config::search::classify_input(trimmed),
                    buffr_config::search::InputKind::Url | buffr_config::search::InputKind::Host
                ) {
                    debug!(text = trimmed, "paste_url: clipboard isn't a URL — no-op");
                    return;
                }
                let resolved = buffr_config::search::resolve_input(trimmed, &self.search_config);
                if let Err(err) = host.open_tab_at(&resolved, insert_idx) {
                    warn!(error = %err, url = %resolved, "paste_url: open_tab_at failed");
                }
            }
            A::FocusFirstInput => {
                // User gesture — allow the next focusin to enter Insert.
                self.insert_intent_at = Some(Instant::now());
                if let Some(ref id) = self.last_focused_field.clone() {
                    // Re-focus the previously-focused field by its stable
                    // buffr ID rather than always jumping to the first one.
                    host.run_edit_focus(id);
                } else {
                    // No prior focus on this page — fall back to first-input.
                    host.dispatch(action);
                }
            }
            A::ExitInsertMode => {
                // Run the JS blur via the host arm.
                host.dispatch(action);
                // Clear local edit state synchronously — don't wait for the
                // JS-driven blur event to arrive.
                self.edit_focus = EditFocus::None;
                if let Ok(mut e) = self.engine.lock() {
                    e.set_mode(PageMode::Normal);
                }
                self.refresh_title();
                self.request_redraw();
            }
            A::YankSelection => {
                host.dispatch(action);
                if let Ok(mut e) = self.engine.lock() {
                    e.set_mode(PageMode::Normal);
                }
                self.refresh_title();
                self.request_redraw();
            }
            _ => host.dispatch(action),
        }
    }

    /// Close the active tab. If it was the last one, signal the
    /// caller to exit. Returns `true` if more tabs remain.
    ///
    /// Closing a *pinned* active tab is gated through the
    /// confirmation overlay: if no confirmation is currently pending,
    /// arm one and return without closing. The user's response (y or
    /// the Yes button) reaches `confirm_close_now` which calls this
    /// path again with the confirmation already cleared.
    fn close_active_tab_or_exit(&mut self) -> bool {
        let Some(host) = self.host.as_ref() else {
            return false;
        };
        if self.confirm_close_pinned.is_none()
            && let Some(t) = host.active_tab()
            && t.pinned
        {
            self.confirm_close_pinned = Some(t.id);
            self.mark_chrome_dirty();
            self.request_redraw();
            return true;
        }
        match host.close_active() {
            Ok(true) => true,
            Ok(false) => {
                info!("tab_close: last tab gone — saving session and exiting");
                self.save_session_now();
                std::process::exit(0);
            }
            Err(err) => {
                warn!(error = %err, "tab_close: failed");
                true
            }
        }
    }

    /// Resolve the close-pinned confirmation. `confirm = true` clears
    /// the prompt and finishes the close; `false` just dismisses.
    fn resolve_pinned_close(&mut self, confirm: bool) {
        let Some(target) = self.confirm_close_pinned.take() else {
            return;
        };
        self.mark_chrome_dirty();
        self.request_redraw();
        if !confirm {
            return;
        }
        // Close the recorded tab even if focus shifted in between.
        if let Some(host) = self.host.as_ref() {
            let only = host.tab_count() <= 1;
            let _ = host.close_tab(target);
            if only {
                info!("tab_close: last tab gone — saving session and exiting");
                self.save_session_now();
                std::process::exit(0);
            }
            self.refresh_tab_strip();
            self.mark_session_dirty();
        }
    }

    /// Persist the live tab list synchronously. Called on graceful
    /// shutdown paths (last-tab-close, `:q`, `Ctrl-C`) and from
    /// `about_to_wait` when `session_dirty` is set. No-op when the
    /// in-memory snapshot already matches and `session_dirty` is false.
    fn save_session_now(&mut self) {
        let Some(path) = self.session_path.as_ref() else {
            return;
        };
        let Some(host) = self.host.as_ref() else {
            return;
        };
        let summaries = host.tabs_summary();
        let active = host.active_index();
        let ids: Vec<buffr_core::TabId> = summaries.iter().map(|t| t.id).collect();
        let url = host.active_tab_live_url();

        // Skip if nothing changed and no external dirty signal.
        if !self.session_dirty
            && active == self.last_session_active
            && ids == self.last_session_tab_ids
            && url == self.last_session_url
        {
            return;
        }

        let s = session::Session::from_tabs_with_active(
            summaries.iter().map(|t| (t.url.as_str(), t.pinned)),
            active,
        );
        if let Err(err) = session::write(path, &s) {
            warn!(error = %err, "session: write failed");
            return;
        }

        // Update snapshots and clear dirty flag + debounce clock.
        self.last_session_url = url;
        self.last_session_active = active;
        self.last_session_tab_ids = ids;
        self.session_dirty = false;
        self.session_dirty_since = None;
    }

    /// Mark the session as needing a flush. Call this at any site that
    /// mutates tab state outside the `about_to_wait` URL-poll path.
    fn mark_session_dirty(&mut self) {
        self.session_dirty = true;
        // Start the debounce clock only on the transition into dirty.
        // Re-arming on every call would let high-frequency callers
        // (e.g. the 250 ms URL/active-index poll) push the deadline
        // forward indefinitely so the flush would never fire.
        if self.session_dirty_since.is_none() {
            self.session_dirty_since = Some(Instant::now());
        }
    }

    /// Open any extra `--new-tab` URLs after the homepage / session
    /// has been initialised. Drained once per `resumed` tick.
    fn open_pending_tabs(&mut self) {
        let Some(host) = self.host.as_ref() else {
            return;
        };
        // Restored session first — these were saved in the previous
        // run's tab order. The first one is already loaded as the
        // initial tab via `BrowserHost::new`; the rest open in the
        // background so the user lands on tab 0.
        let session = std::mem::take(&mut self.pending_session_tabs);
        for (i, (url, pinned)) in session.iter().enumerate() {
            if i == 0 {
                // The initial `BrowserHost::new` already loaded tab 0
                // with `homepage`. Navigate the active tab there
                // instead of opening a new one so we don't end up
                // with a stray homepage tab.
                if let Err(err) = host.navigate(url) {
                    warn!(error = %err, %url, "session: navigate first tab failed");
                }
                if *pinned && let Some(active) = host.active_tab() {
                    host.set_pinned(active.id, true);
                }
                continue;
            }
            match host.open_tab_background(url) {
                Ok(id) => {
                    if *pinned {
                        // The new tab is in the background, so the
                        // pin must target it by id rather than the
                        // currently-active tab.
                        host.set_pinned(id, true);
                    }
                }
                Err(err) => warn!(error = %err, %url, "session: open_tab failed"),
            }
        }
        // Restore the active tab from the session, if any.
        if let Some(idx) = self.pending_session_active.take() {
            let summaries = host.tabs_summary();
            if let Some(tab) = summaries.get(idx) {
                host.select_tab(tab.id);
            }
        }
        // CLI `--new-tab` URLs append after the session.
        let cli_tabs = std::mem::take(&mut self.pending_new_tabs);
        for url in cli_tabs {
            if let Err(err) = host.open_tab_background(&url) {
                warn!(error = %err, %url, "new-tab: open_tab failed");
            }
        }
    }

    /// Refresh the tab-strip render input from the host's current
    /// tab list. Cheap; runs every `about_to_wait` tick.
    fn refresh_tab_strip(&mut self) {
        let Some(host) = self.host.as_ref() else {
            return;
        };
        let summaries = host.tabs_summary();
        let active = host.active_index();
        let mut ids = Vec::with_capacity(summaries.len());
        let tabs = summaries
            .into_iter()
            .map(|t| {
                ids.push(t.id);
                TabView {
                    title: t.title,
                    progress: t.progress,
                    pinned: t.pinned,
                    private: t.private,
                }
            })
            .collect();
        self.tab_ids = ids;
        let tabs_changed = tabs != self.tab_strip.tabs || active != self.tab_strip.active;
        self.tab_strip.tabs = tabs;
        self.tab_strip.active = active;
        if tabs_changed {
            self.mark_chrome_dirty();
        }
    }

    fn refresh_title(&mut self) {
        let (mode, count) = match self.engine.lock() {
            Ok(e) => (e.mode(), e.count_buffer()),
            Err(_) => (PageMode::Normal, None),
        };
        let label = mode_label(mode);
        self.current_mode_label = label;
        let url = self.statusline.url.clone();
        let title = self.title_for(label, &url);
        let title_changed = title != self.current_title;
        if title_changed {
            self.current_title = title.clone();
            if let Some(window) = self.window.as_ref() {
                window.set_title(&title);
            }
        }
        // Only request a redraw when the visible chrome state actually
        // changed. Previously this fired on every call, including pass-
        // through key events in Insert mode where mode/count/url stay
        // identical — CEF emits its own on_paint after processing the
        // key which already triggers a redraw, so the pre-CEF redraw
        // here just painted stale OSR pixels and doubled GPU work.
        let chrome_changed =
            self.statusline.mode != mode || self.statusline.count_buffer != count || title_changed;
        let leaving_visual = self.statusline.mode == PageMode::Visual && mode != PageMode::Visual;
        self.statusline.mode = mode;
        self.statusline.count_buffer = count;
        if leaving_visual && let Some(host) = self.host.as_ref() {
            // Drop the page's DOM selection so the highlight goes with
            // Visual mode. Any prior YankSelection JS has already been
            // queued in the renderer and runs first; this just collapses
            // what's left.
            host.run_main_frame_js(
                "try { var s = window.getSelection && window.getSelection(); if (s) s.removeAllRanges(); } catch (_) {}",
                "buffr://visual-clear-selection",
            );
        }
        if chrome_changed {
            self.mark_chrome_dirty();
            self.request_redraw();
        }
    }

    fn request_redraw(&self) {
        if let Some(window) = self.window.as_ref() {
            window.request_redraw();
        }
    }

    /// Drain the find-result mailbox into the statusline. Called from
    /// `about_to_wait` so the chrome reflects the latest CEF tick on
    /// the next paint.
    fn pump_find_results(&mut self) {
        if let Some(result) = buffr_core::take_find_result(&self.find_sink) {
            // Preserve the user's query string — `FindResult` only
            // carries counts. If `find_query` is `None` the caller
            // hasn't issued a `start_find` yet (legitimate during
            // shutdown); silently drop the tick.
            let query = self
                .statusline
                .find_query
                .as_ref()
                .map(|s| s.query.clone())
                .or_else(|| self.pending_find.clone());
            if let Some(query) = query {
                self.statusline.find_query = Some(FindStatus {
                    query,
                    current: result.current,
                    total: result.count,
                });
                self.mark_chrome_dirty();
                self.request_redraw();
            }
            tracing::info!(
                count = result.count,
                current = result.current,
                final_update = result.final_update,
                "find: result tick"
            );
        }
    }

    /// If `--find` was passed and the smoke deadline elapsed, kick
    /// the find off exactly once.
    fn maybe_dispatch_find_smoke(&mut self) {
        let Some(deadline) = self.find_smoke_at else {
            return;
        };
        if Instant::now() < deadline {
            return;
        }
        self.find_smoke_at = None;
        if let (Some(host), Some(query)) = (self.host.as_ref(), self.pending_find.take()) {
            tracing::debug!(%query, "find smoke: start_find");
            self.statusline.find_query = Some(FindStatus {
                query: query.clone(),
                current: 0,
                total: 0,
            });
            host.start_find(&query, true);
        }
    }

    fn paint_chrome(&mut self) {
        self.paint_chrome_with(None);
    }

    /// Two-finger horizontal-swipe back/forward gesture detector.
    /// Call once per touchpad `PixelDelta` event with the raw winit
    /// delta in screen pixels. Returns `Some(HistoryBack | HistoryForward)`
    /// the first time a gesture commits; subsequent events of the same
    /// gesture are bounded by `swipe_committed` (caller swallows them).
    /// A gesture is bounded by `GAP` of inactivity.
    ///
    /// Direction: positive winit `PixelDelta.x` = swipe RIGHT → back.
    /// Negative = swipe LEFT → forward. Mirrors Chrome/Safari macOS
    /// convention (verified on Linux Wayland touchpad with natural
    /// scrolling enabled — sign matches the physical gesture there).
    fn detect_swipe(&mut self, dx: f32, dy: f32) -> Option<buffr_modal::PageAction> {
        const GAP: Duration = Duration::from_millis(200);
        // Raw px thresholds — touchpad 2-finger swipes deliver ~5-15px
        // per event at 60Hz, so 150px = ~10-30 events of intent.
        const HORIZ_THRESHOLD: f32 = 150.0;
        const HORIZ_DOMINANCE: f32 = 2.0;

        let now = Instant::now();
        let resumed = self
            .swipe_last_at
            .map(|t| now.duration_since(t) > GAP)
            .unwrap_or(true);
        if resumed {
            self.swipe_accum_x = 0.0;
            self.swipe_accum_y = 0.0;
            self.swipe_committed = false;
        }
        self.swipe_last_at = Some(now);
        self.swipe_accum_x += dx;
        self.swipe_accum_y += dy;

        if self.swipe_committed {
            return None;
        }
        let ax = self.swipe_accum_x.abs();
        let ay = self.swipe_accum_y.abs();
        if ax >= HORIZ_THRESHOLD && ax > HORIZ_DOMINANCE * ay {
            self.swipe_committed = true;
            let action = if self.swipe_accum_x > 0.0 {
                buffr_modal::PageAction::HistoryBack
            } else {
                buffr_modal::PageAction::HistoryForward
            };
            tracing::debug!(
                accum_x = self.swipe_accum_x,
                accum_y = self.swipe_accum_y,
                ?action,
                "swipe gesture committed",
            );
            return Some(action);
        }
        None
    }

    /// Synthesize wheel-momentum decay frames after high-res input
    /// stops. Called from `about_to_wait` at ~60 Hz when the event loop
    /// is otherwise idle. Constants tuned by feel; tweak `DECAY` toward
    /// 1.0 for a longer tail or down for snappier stops.
    fn tick_wheel_momentum(&mut self) {
        let Some(last_at) = self.osr_wheel_last_at else {
            return;
        };
        // Grace window: real wheel events typically arrive every ~6 ms.
        // Don't decay until the input has been quiet for ≥ 30 ms so
        // momentum doesn't fight a still-active scroll gesture.
        if last_at.elapsed() < Duration::from_millis(30) {
            return;
        }
        const DECAY: f32 = 0.92;
        const MIN_VEL: f32 = 8.0;
        self.osr_wheel_velocity.0 *= DECAY;
        self.osr_wheel_velocity.1 *= DECAY;
        if self.osr_wheel_velocity.0.abs() < MIN_VEL {
            self.osr_wheel_velocity.0 = 0.0;
        }
        if self.osr_wheel_velocity.1.abs() < MIN_VEL {
            self.osr_wheel_velocity.1 = 0.0;
        }
        let dx = self.osr_wheel_velocity.0 as i32;
        let dy = self.osr_wheel_velocity.1 as i32;
        if dx == 0 && dy == 0 {
            self.osr_wheel_last_at = None;
            return;
        }
        if let Some(host) = self.host.as_ref()
            && host.mode() == buffr_core::HostMode::Osr
        {
            let mods = winit_mods_to_cef(&self.modifiers);
            let (bx, by) = self.osr_cursor;
            tracing::trace!(dx, dy, "input: wheel_momentum -> CEF");
            host.osr_mouse_wheel(bx, by, dx, dy, mods);
        }
    }

    /// Paint chrome at explicit dims when caller has fresher size info
    /// than `window.inner_size()` returns. Wayland's configure handshake
    /// can leave `window.inner_size()` reporting the previous dims at
    /// the moment `WindowEvent::Resized` fires; passing the event's
    /// `new_size` directly avoids painting at stale width/height.
    fn paint_chrome_with(&mut self, override_size: Option<(u32, u32)>) {
        let Some(window) = self.window.as_ref() else {
            return;
        };
        let inner = window.inner_size();
        let (width, height) = match override_size {
            Some((w, h)) => (w.max(1), h.max(1)),
            None => (inner.width.max(1), inner.height.max(1)),
        };

        // Precompute geometry before the renderer call — helpers need `&self`.
        let tab_y = self.tab_strip_y(height);
        let notice_y = self.download_notice_y();
        let current_notice = peek_download_notice(&self.download_notice_queue);
        let (_, browser_y, browser_w, browser_h) = self.cef_child_rect(width, height);

        // Acquire the latest OSR pixels by swapping our scratch buffer
        // with the SharedOsrFrame's pixel Vec. Lock duration is the cost
        // of a Vec<u8> swap (three usize writes) — negligible. CEF's
        // on_paint thread, when it next fires, gets the empty Vec we put
        // in and resizes it; on_paint already handles len mismatch via
        // the resize check, so no panic. After this block, self.osr_scratch
        // owns the freshest CEF pixels and self.host's frame.pixels is empty.
        let osr_meta: Option<(u32, u32, u64)> = if let Some(host) = self.host.as_ref()
            && host.mode() == buffr_core::HostMode::Osr
        {
            if let Ok(mut frame) = host.osr_frame().lock() {
                if frame.width > 0 && frame.height > 0 && !frame.pixels.is_empty() {
                    std::mem::swap(&mut self.osr_scratch, &mut frame.pixels);
                    Some((frame.width, frame.height, frame.generation))
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };

        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };

        // Resize bumps chrome_generation via the caller's resize event;
        // the renderer itself tracks whether it needs to reallocate.
        renderer.resize(width, height);

        let chrome_dirty = self.chrome_generation != self.last_painted_chrome_gen;

        // Clone/snapshot values needed in the chrome paint closure.
        let statusline = self.statusline.clone();
        let tab_strip = self.tab_strip.clone();
        let confirm_close_pinned = self.confirm_close_pinned;
        let permissions_prompt = self.permissions_prompt.clone();
        let overlay_data = self.overlay.as_ref().map(|o| o.input().clone());

        let frame_start = Instant::now();

        // Build the OsrUpload from our just-swapped scratch buffer.
        let new_osr_generation;
        let res = if let Some((osr_w, osr_h, osr_gen)) = osr_meta {
            new_osr_generation = osr_gen;
            // dst_rect uses the live browser rect (not min'd against the
            // stale OSR dims). The renderer GPU-stretches the OSR texture
            // to fill it, so when CEF's buffer lags the window resize the
            // stale frame visually scales up instead of letterboxing.
            let osr_upload = crate::render::OsrUpload {
                pixels: &self.osr_scratch,
                width: osr_w,
                height: osr_h,
                generation: osr_gen,
                dst_rect: (0, browser_y, browser_w, browser_h),
            };
            renderer.frame(
                chrome_dirty,
                |buf, w, _h| {
                    paint_chrome_strips(
                        buf,
                        w,
                        height,
                        &statusline,
                        &tab_strip,
                        tab_y,
                        notice_y,
                        current_notice.as_ref(),
                        confirm_close_pinned,
                        permissions_prompt.as_ref(),
                        overlay_data.as_ref(),
                    );
                },
                Some(osr_upload),
            )
        } else {
            new_osr_generation = self.last_osr_generation;
            renderer.frame(
                chrome_dirty,
                |buf, w, _h| {
                    paint_chrome_strips(
                        buf,
                        w,
                        height,
                        &statusline,
                        &tab_strip,
                        tab_y,
                        notice_y,
                        current_notice.as_ref(),
                        confirm_close_pinned,
                        permissions_prompt.as_ref(),
                        overlay_data.as_ref(),
                    );
                },
                None,
            )
        };

        self.last_osr_generation = new_osr_generation;
        if chrome_dirty {
            self.last_painted_chrome_gen = self.chrome_generation;
        }

        let total_us = frame_start.elapsed().as_micros() as u64;
        tracing::trace!(
            win_w = width,
            win_h = height,
            chrome_dirty,
            gen = new_osr_generation,
            total_us,
            "paint_chrome",
        );
        if total_us > 16_000 {
            tracing::debug!(
                win_w = width,
                win_h = height,
                chrome_dirty,
                total_us,
                "paint_chrome: slow frame",
            );
        }

        if let Err(err) = res {
            warn!(error = %err, "wgpu frame failed");
        }
    }

    /// Compute the CEF page rect for the current overlay state.
    ///
    /// Vertical layout (top → bottom):
    ///
    /// 1. Download notice strip (`DOWNLOAD_NOTICE_HEIGHT`, when queued)
    /// 2. Tab strip (always, `TAB_STRIP_HEIGHT` px)
    /// 3. CEF page area  ← confirm/permissions/omnibar popups float over this
    /// 4. Statusline (always, `STATUSLINE_HEIGHT` px)
    fn cef_child_rect(&self, full_w: u32, full_h: u32) -> (u32, u32, u32, u32) {
        let status_h = STATUSLINE_HEIGHT.min(full_h);
        let remaining_after_status = full_h.saturating_sub(status_h);
        let tab_h = TAB_STRIP_HEIGHT.min(remaining_after_status);
        let remaining_after_tabs = remaining_after_status.saturating_sub(tab_h);
        let notice_h = if buffr_core::download_notice_queue_len(&self.download_notice_queue) > 0 {
            DOWNLOAD_NOTICE_HEIGHT.min(remaining_after_tabs)
        } else {
            0
        };
        let remaining_after_notice = remaining_after_tabs.saturating_sub(notice_h);
        let cef_w = full_w.max(1);
        let cef_h = remaining_after_notice.max(1);
        let cef_y = notice_h + tab_h;
        (0, cef_y, cef_w, cef_h)
    }

    /// The pixel row at which the tab strip begins (top of the
    /// `TAB_STRIP_HEIGHT` band). Mirrors the math in
    /// [`Self::cef_child_rect`] without depending on the CEF rect
    /// itself. The overlay is a floating popup and does not affect
    /// the tab strip position.
    /// Hit-test the current cursor position against the tab strip.
    /// Returns the index of the tab under the cursor, or `None` if the
    /// cursor isn't in the strip or the tab list is empty.
    fn hit_test_tab_strip(&self) -> Option<usize> {
        let window = self.window.as_ref()?;
        let size = window.inner_size();
        let full_w = size.width.max(1);
        let full_h = size.height.max(1);
        let tab_y = self.tab_strip_y(full_h);
        let tab_y_end = tab_y + TAB_STRIP_HEIGHT;
        let (_, cef_y, _, _) = self.cef_child_rect(full_w, full_h);
        let wx = self.osr_cursor.0 as u32;
        let wy = (self.osr_cursor.1 + cef_y as i32).max(0) as u32;
        if wy < tab_y || wy >= tab_y_end || self.tab_ids.is_empty() {
            return None;
        }
        // Mirror the layout in `TabStrip::paint`: pinned tabs occupy
        // PINNED_TAB_WIDTH each, unpinned tabs share whatever's left.
        const GUTTER: u32 = 4;
        let pinned_count = self.tab_strip.tabs.iter().filter(|t| t.pinned).count() as u32;
        let total_count = self.tab_ids.len() as u32;
        let unpinned_count = total_count.saturating_sub(pinned_count);
        let pinned_total_w = pinned_count * buffr_ui::tab_strip::PINNED_TAB_WIDTH;
        let gutter_total = (total_count + 1) * GUTTER;
        let avail_for_unpinned = full_w
            .saturating_sub(pinned_total_w)
            .saturating_sub(gutter_total);
        let raw_w = avail_for_unpinned.checked_div(unpinned_count).unwrap_or(0);
        let unpinned_w = raw_w.clamp(buffr_ui::MIN_TAB_WIDTH, buffr_ui::MAX_TAB_WIDTH);

        if wx < GUTTER {
            return None;
        }
        // Walk the pills left-to-right and pick the one whose rect
        // contains `wx`.
        let mut x = GUTTER;
        for (i, tab) in self.tab_strip.tabs.iter().enumerate() {
            let pill_w = if tab.pinned {
                buffr_ui::tab_strip::PINNED_TAB_WIDTH
            } else {
                unpinned_w
            };
            let right = x + pill_w;
            if wx >= x && wx < right {
                return Some(i);
            }
            x = right + GUTTER;
        }
        None
    }

    fn tab_strip_y(&self, full_h: u32) -> u32 {
        let notice_h = if buffr_core::download_notice_queue_len(&self.download_notice_queue) > 0 {
            DOWNLOAD_NOTICE_HEIGHT
        } else {
            0
        };
        notice_h.min(full_h)
    }

    /// Top-of-window y for the download notice strip. Sits at the
    /// top of the window above the permissions prompt. The overlay is
    /// a floating popup and does not affect this position.
    fn download_notice_y(&self) -> u32 {
        0
    }

    /// Re-issue the CEF resize call for the current window dimensions.
    /// Called whenever the overlay opens or closes so the page region
    /// re-flows to fill the freed space.
    fn resync_cef_rect(&self) {
        let Some(window) = self.window.as_ref() else {
            return;
        };
        let size = window.inner_size();
        let (_x, _y, w, h) = self.cef_child_rect(size.width.max(1), size.height.max(1));
        if let Some(host) = self.host.as_ref() {
            host.resize(w, h);
        }
    }

    fn open_command_line(&mut self) {
        self.overlay = Some(OverlayState::Command(InputBar::with_prefix(":")));
        self.refresh_overlay_suggestions();
        self.mark_chrome_dirty();
        self.request_redraw();
    }

    fn open_omnibar(&mut self) {
        let mut bar = InputBar::with_prefix("> ");
        // Pre-populate with the current page URL so the user can edit
        // it in place — Vimium / qutebrowser convention. Internal
        // buffr:// URLs (new-tab page, etc.) start empty so the user
        // can type a fresh query immediately.
        //
        // Query the host directly — `statusline.url` is updated by a
        // 250ms-throttled poll, so it can lag a tab switch and pre-fill
        // the omnibar with the previous tab's URL.
        let url = self
            .host
            .as_ref()
            .map(|h| h.active_tab_live_url())
            .unwrap_or_default();
        if !url.starts_with("buffr:") {
            bar.buffer = url;
            bar.cursor = bar.buffer.len();
        }
        self.overlay = Some(OverlayState::Omnibar(bar));
        self.refresh_overlay_suggestions();
        self.mark_chrome_dirty();
        self.request_redraw();
    }

    fn open_find(&mut self, forward: bool) {
        let prefix = if forward { "/ " } else { "? " };
        let bar = InputBar::with_prefix(prefix);
        self.overlay = Some(OverlayState::Find { forward, bar });
        if let Ok(mut e) = self.engine.lock() {
            e.set_mode(PageMode::Command);
        }
        self.refresh_overlay_suggestions();
        self.mark_chrome_dirty();
        self.request_redraw();
    }

    fn close_overlay(&mut self) {
        if self.overlay.is_none() {
            return;
        }
        // Cancelling a `/` / `?` overlay tears down the live highlight
        // so a half-typed query doesn't leave the page lit up.
        let was_find = matches!(self.overlay, Some(OverlayState::Find { .. }));
        self.find_live_due = None;
        if was_find {
            if let Some(host) = self.host.as_ref() {
                host.stop_find();
            }
            self.statusline.find_query = None;
        }
        self.overlay = None;
        self.mark_chrome_dirty();
        // Engine flips back to Normal so the modal trie resumes.
        if let Ok(mut e) = self.engine.lock() {
            e.set_mode(PageMode::Normal);
        }
        // If this overlay was the auto-omnibar of a freshly-opened
        // tab (`o` / `O`), close that tab on cancel — but only if
        // there'd be at least one tab left.
        if let Some(tab_id) = self.cancel_closes_tab.take()
            && let Some(host) = self.host.as_ref()
            && host.tab_count() > 1
        {
            let _ = host.close_tab(tab_id);
            self.refresh_tab_strip();
            self.mark_session_dirty();
        }
        // Overlay is a floating popup — no CEF resize on toggle.
        self.refresh_title();
    }

    /// Recompute the suggestion list for the current overlay buffer.
    /// Called on every keystroke; SQLite searches at this depth (8
    /// rows from each store) cost ~1ms on a warm cache, well below
    /// human typing rates.
    fn refresh_overlay_suggestions(&mut self) {
        let Some(overlay) = self.overlay.as_mut() else {
            return;
        };
        let buffer = overlay.input().buffer.clone();
        let suggestions = match overlay {
            OverlayState::Command(_) => self.command_suggestions(&buffer),
            OverlayState::Omnibar(_) => self.omnibar_suggestions(&buffer),
            OverlayState::Find { .. } => {
                // Live-find: every keystroke pushes the deadline out by
                // `FIND_LIVE_DEBOUNCE_MS`; about_to_wait fires start_find
                // once the user pauses.
                self.find_live_due =
                    Some(Instant::now() + Duration::from_millis(FIND_LIVE_DEBOUNCE_MS));
                Vec::new()
            }
        };
        // Re-borrow the overlay since `self.command_suggestions` /
        // `omnibar_suggestions` need `&self`.
        if let Some(overlay) = self.overlay.as_mut() {
            overlay.input_mut().set_suggestions(suggestions);
        }
    }

    /// Run a live-find tick if the debounce deadline has elapsed.
    /// Called from `about_to_wait`. Cleared once fired so a second
    /// tick won't repeat without another keystroke.
    fn maybe_dispatch_find_live(&mut self) {
        let Some(due) = self.find_live_due else {
            return;
        };
        if Instant::now() < due {
            return;
        }
        self.find_live_due = None;
        let Some(OverlayState::Find { forward, bar }) = self.overlay.as_ref() else {
            return;
        };
        let forward = *forward;
        let query = bar.current_value().trim().to_string();
        let Some(host) = self.host.as_ref() else {
            return;
        };
        if query.is_empty() {
            host.stop_find();
            self.statusline.find_query = None;
            self.mark_chrome_dirty();
            return;
        }
        host.start_find(&query, forward);
        self.statusline.find_query = Some(FindStatus {
            query,
            current: 0,
            total: 0,
        });
        self.mark_chrome_dirty();
    }

    fn command_suggestions(&self, buffer: &str) -> Vec<Suggestion> {
        let needle = buffer.trim();
        buffr_core::cmdline::COMMAND_NAMES
            .iter()
            .filter(|name| needle.is_empty() || name.starts_with(needle))
            .take(buffr_ui::MAX_SUGGESTIONS)
            .map(|name| Suggestion {
                display: format!(":{name}"),
                value: (*name).to_string(),
                kind: SuggestionKind::Command,
            })
            .collect()
    }

    fn omnibar_suggestions(&self, buffer: &str) -> Vec<Suggestion> {
        let needle = buffer.trim();
        if needle.is_empty() {
            return Vec::new();
        }
        let mut out: Vec<Suggestion> = Vec::with_capacity(buffr_ui::MAX_SUGGESTIONS);
        let mut seen_urls = std::collections::HashSet::<String>::new();

        // History first.
        if let Ok(rows) = self.history.search(needle, 8) {
            for row in rows {
                if seen_urls.insert(row.url.clone()) {
                    let display = match row.title.as_deref() {
                        Some(t) if !t.is_empty() => format!("{t} — {}", row.url),
                        _ => row.url.clone(),
                    };
                    out.push(Suggestion {
                        display,
                        value: row.url,
                        kind: SuggestionKind::History,
                    });
                    if out.len() >= buffr_ui::MAX_SUGGESTIONS {
                        return out;
                    }
                }
            }
        }
        // Bookmarks next.
        if let Ok(rows) = self.bookmarks.search(needle) {
            for bm in rows.into_iter().take(8) {
                if seen_urls.insert(bm.url.clone()) {
                    let display = match bm.title.as_deref() {
                        Some(t) if !t.is_empty() => format!("{t} — {}", bm.url),
                        _ => bm.url.clone(),
                    };
                    out.push(Suggestion {
                        display,
                        value: bm.url,
                        kind: SuggestionKind::Bookmark,
                    });
                    if out.len() >= buffr_ui::MAX_SUGGESTIONS {
                        return out;
                    }
                }
            }
        }
        // Search fallback (always last when there's room).
        if out.len() < buffr_ui::MAX_SUGGESTIONS {
            let resolved = buffr_config::resolve_input(needle, &self.search_config);
            if !resolved.is_empty() {
                out.push(Suggestion {
                    display: format!("Search: {needle}"),
                    value: resolved,
                    kind: SuggestionKind::SearchSuggestion,
                });
            }
        }
        out
    }

    /// Route a winit `KeyEvent` to the open overlay. Returns `true` if
    /// the event was consumed (caller skips the engine path).
    fn overlay_handle_key(&mut self, event: &winit::event::KeyEvent) -> bool {
        if self.overlay.is_none() {
            return false;
        }
        // Allow auto-repeat so holding Backspace / arrows / chars in
        // the omnibar fires continuously.
        let chord = match key_event_to_chord_with_repeat(event, self.modifiers) {
            Some(c) => c,
            None => return true, // overlay swallows unmappable keys too
        };
        // Esc / <C-c> cancel. <CR> confirms. Everything else either
        // edits the buffer or moves the selection.
        let mods = chord.modifiers;
        let key = chord.key;
        let is_ctrl = mods.contains(buffr_modal::Modifiers::CTRL)
            && !mods.contains(buffr_modal::Modifiers::SHIFT);

        match (key, is_ctrl) {
            (Key::Named(NamedKey::Esc), _) | (Key::Char('c'), true) => {
                self.close_overlay();
            }
            (Key::Char('u'), true) => {
                if let Some(o) = self.overlay.as_mut() {
                    o.input_mut().handle_clear_line();
                }
                self.refresh_overlay_suggestions();
                self.request_redraw();
            }
            (Key::Char('w'), true) => {
                if let Some(o) = self.overlay.as_mut() {
                    o.input_mut().handle_delete_word();
                }
                self.refresh_overlay_suggestions();
                self.request_redraw();
            }
            (Key::Char('v'), true) => {
                // Paste clipboard text into the overlay input. Drop CR/LF
                // so a multiline clipboard doesn't leak past the single
                // input row.
                if let Some(host) = self.host.as_ref()
                    && let Some(text) = host.clipboard_text()
                    && let Some(o) = self.overlay.as_mut()
                {
                    for c in text.chars() {
                        if c == '\n' || c == '\r' {
                            continue;
                        }
                        o.input_mut().handle_text(c);
                    }
                    self.refresh_overlay_suggestions();
                    self.request_redraw();
                }
            }
            (Key::Named(NamedKey::CR), _) => {
                self.confirm_overlay();
            }
            (Key::Named(NamedKey::Tab), _) | (Key::Named(NamedKey::Down), _) => {
                if let Some(o) = self.overlay.as_mut() {
                    o.input_mut().handle_down();
                }
                self.request_redraw();
            }
            (Key::Named(NamedKey::BackTab), _) | (Key::Named(NamedKey::Up), _) => {
                if let Some(o) = self.overlay.as_mut() {
                    o.input_mut().handle_up();
                }
                self.request_redraw();
            }
            (Key::Named(NamedKey::Left), _) => {
                if let Some(o) = self.overlay.as_mut() {
                    o.input_mut().handle_left();
                }
                self.request_redraw();
            }
            (Key::Named(NamedKey::Right), _) => {
                if let Some(o) = self.overlay.as_mut() {
                    o.input_mut().handle_right();
                }
                self.request_redraw();
            }
            (Key::Named(NamedKey::BS), _) => {
                if let Some(o) = self.overlay.as_mut() {
                    o.input_mut().handle_back();
                }
                self.refresh_overlay_suggestions();
                self.request_redraw();
            }
            (Key::Named(NamedKey::Space), _) => {
                // winit reports space as a Named key, not Char(' ').
                // The omnibar is text input — space is just a literal.
                if let Some(o) = self.overlay.as_mut() {
                    o.input_mut().handle_text(' ');
                }
                self.refresh_overlay_suggestions();
                self.request_redraw();
            }
            (Key::Char(c), false) => {
                if let Some(o) = self.overlay.as_mut() {
                    o.input_mut().handle_text(c);
                }
                self.refresh_overlay_suggestions();
                self.request_redraw();
            }
            _ => {
                // Unhandled chord while overlay open — swallow so the
                // engine doesn't see it. Phase 3b may surface a beep.
            }
        }
        self.mark_chrome_dirty();
        true
    }

    fn confirm_overlay(&mut self) {
        let Some(overlay) = self.overlay.take() else {
            return;
        };
        // User confirmed — keep the freshly-opened tab around.
        self.cancel_closes_tab = None;
        // Submit path runs `start_find` directly; don't let a pending
        // live tick fire a duplicate after dispatch.
        self.find_live_due = None;
        // Engine flips back regardless of dispatch outcome.
        if let Ok(mut e) = self.engine.lock() {
            e.set_mode(PageMode::Normal);
        }
        match overlay {
            OverlayState::Command(bar) => self.dispatch_command(&bar),
            OverlayState::Omnibar(bar) => self.dispatch_omnibar(&bar),
            OverlayState::Find { forward, bar } => self.dispatch_find(&bar, forward),
        }
        self.resync_cef_rect();
        self.refresh_title();
    }

    fn dispatch_command(&mut self, bar: &InputBar) {
        // If the user hit Enter on a selected suggestion, prefer that
        // value (the bare command name) over the typed buffer.
        let raw = bar.current_value();
        let parsed = parse_cmdline(raw);
        match parsed {
            Command::Quit => {
                // Vim-flavoured: `:q` closes the active tab; only the
                // very last tab quits the app. Mirrors `<C-w>c`. To
                // force-quit the whole app from the command line use
                // `:q!` (not yet implemented) — for now `:q` on the
                // last tab triggers the same exit path.
                tracing::info!("cmdline: quit — closing active tab");
                self.close_active_tab_or_exit();
            }
            Command::Reload => {
                self.dispatch_action(&buffr_modal::PageAction::Reload);
            }
            Command::Back => {
                self.dispatch_action(&buffr_modal::PageAction::HistoryBack);
            }
            Command::Forward => {
                self.dispatch_action(&buffr_modal::PageAction::HistoryForward);
            }
            Command::Open(url) => {
                if let Some(host) = self.host.as_ref() {
                    if let Err(err) = host.navigate(&url) {
                        warn!(error = %err, %url, "open: navigate failed");
                    }
                } else {
                    warn!(%url, "open: no host yet");
                }
            }
            Command::TabNew => {
                let url = self.homepage.clone();
                if let Some(host) = self.host.as_ref()
                    && let Err(err) = host.open_tab(&url)
                {
                    warn!(error = %err, %url, "cmdline :tabnew failed");
                }
            }
            Command::Set { key, value } => {
                self.apply_set(&key, &value);
            }
            Command::Find(query) => {
                if let Some(host) = self.host.as_ref() {
                    self.statusline.find_query = Some(FindStatus {
                        query: query.clone(),
                        current: 0,
                        total: 0,
                    });
                    host.start_find(&query, true);
                }
            }
            Command::Bookmark { tags } => {
                let url = self.statusline.url.clone();
                if url.is_empty() {
                    tracing::warn!(":bookmark — no current URL");
                } else {
                    let tag_refs: Vec<&str> = tags.iter().map(String::as_str).collect();
                    match self.bookmarks.add(&url, None, &tag_refs) {
                        Ok(_) => {
                            tracing::debug!(%url, ?tags, "bookmark added");
                            // Phase 6 telemetry: count one bookmark
                            // creation. `:bookmark` is the only path
                            // that calls `Bookmarks::add` from a user
                            // action; the Netscape importer fires its
                            // own loop and is intentionally excluded
                            // from this counter (importer is bulk).
                            self.counters.increment(buffr_core::KEY_BOOKMARKS_ADDED);
                        }
                        Err(err) => tracing::warn!(error = %err, %url, "bookmark failed"),
                    }
                }
            }
            Command::DevTools => {
                self.dispatch_action(&buffr_modal::PageAction::OpenDevTools);
            }
            Command::Unknown(s) => {
                tracing::warn!(input = %s, "cmdline: unknown command");
            }
        }
    }

    fn apply_set(&mut self, key: &str, value: &str) {
        match key {
            "zoom" => match value {
                "in" => self.dispatch_action(&buffr_modal::PageAction::ZoomIn),
                "out" => self.dispatch_action(&buffr_modal::PageAction::ZoomOut),
                "reset" | "0" => self.dispatch_action(&buffr_modal::PageAction::ZoomReset),
                other => tracing::warn!(value = %other, ":set zoom — expected in/out/reset"),
            },
            other => tracing::warn!(key = %other, value, ":set — unknown key"),
        }
    }

    /// Route a keystroke to the active hint session, if any. Returns
    /// `true` if the key was consumed.
    ///
    /// Esc cancels. Backspace pops the typed buffer. Printable ASCII
    /// chars (no Ctrl / Alt / Meta) are fed to `feed_hint_key`. Every
    /// other chord is silently swallowed so the modal trie can't fire
    /// on `j` / `k` etc. while a session is live.
    fn hint_mode_handle_key(&mut self, event: &winit::event::KeyEvent) -> bool {
        let Some(host) = self.host.as_ref() else {
            return false;
        };
        if !host.is_hint_mode() {
            return false;
        }
        let chord = match key_event_to_chord(event, self.modifiers) {
            Some(c) => c,
            None => return true,
        };
        let mods = chord.modifiers;
        let plain = !mods.contains(buffr_modal::Modifiers::CTRL)
            && !mods.contains(buffr_modal::Modifiers::ALT)
            && !mods.contains(buffr_modal::Modifiers::SUPER);
        match chord.key {
            Key::Named(NamedKey::Esc) => {
                host.cancel_hint();
                self.exit_hint_mode();
            }
            Key::Named(NamedKey::BS) => {
                if let Some(action) = host.backspace_hint() {
                    self.handle_hint_action(action);
                }
            }
            Key::Char(c) if plain => {
                if let Some(action) = host.feed_hint_key(c) {
                    self.handle_hint_action(action);
                }
            }
            _ => {
                // Unhandled chord while hint mode is active — swallow.
            }
        }
        self.refresh_title();
        self.request_redraw();
        true
    }

    fn handle_hint_action(&mut self, action: HintAction) {
        match action {
            HintAction::Filter => {
                // Session continues; statusline picks up new typed.
            }
            HintAction::Click(_) | HintAction::OpenInBackground(_) => {
                self.exit_hint_mode();
            }
            HintAction::Cancel => {
                self.exit_hint_mode();
            }
        }
    }

    fn exit_hint_mode(&mut self) {
        if let Ok(mut e) = self.engine.lock() {
            e.set_mode(PageMode::Normal);
        }
        self.statusline.hint_state = None;
        self.mark_chrome_dirty();
    }

    // ---- Edit-mode plumbing ---------------------------------------------

    /// Drain queued edit-focus events and update `self.edit_focus`.
    fn drain_edit_focus_events(&mut self) {
        let mut mode_changed = false;
        for ev in drain_edit_events(&self.edit_sink) {
            match ev {
                EditConsoleEvent::Focus {
                    field_id, ref kind, ..
                } => {
                    // Browser UX: clicking/tabbing to an input auto-enters
                    // Insert mode. A spurious re-focus for the already-active
                    // field must not clobber the existing state.
                    let already_editing = matches!(
                        &self.edit_focus,
                        EditFocus::Editing { field_id: f } if *f == field_id
                    );
                    tracing::debug!(
                        %field_id,
                        ?kind,
                        already_editing,
                        "drain_edit_focus_events: Focus received"
                    );
                    // Only enter Insert mode if a recent user gesture
                    // (left-click or `i`) preceded this focusin. Pages
                    // that autofocus on load or call `.focus()`
                    // programmatically (e.g. monkeytype's Esc-to-reload
                    // refocuses the test input) get ignored.
                    const INTENT_WINDOW: std::time::Duration =
                        std::time::Duration::from_millis(500);
                    let user_intent = self
                        .insert_intent_at
                        .map(|t| t.elapsed() <= INTENT_WINDOW)
                        .unwrap_or(false);
                    // Tab/Shift+Tab transfer: a Blur from the previously
                    // focused field landed within BLUR_TRANSFER_WINDOW.
                    // Treat the Focus as a continuation of Insert mode.
                    let transfer_window = std::time::Duration::from_millis(BLUR_TRANSFER_WINDOW_MS);
                    let is_transfer = self
                        .pending_blur_at
                        .map(|t| t.elapsed() <= transfer_window)
                        .unwrap_or(false);
                    if !already_editing && (user_intent || is_transfer) {
                        self.insert_intent_at = None;
                        self.pending_blur_at = None;
                        if let Some(host) = self.host.as_ref() {
                            host.run_edit_attach(&field_id);
                        }
                        if let Ok(mut e) = self.engine.lock() {
                            e.set_mode(buffr_modal::PageMode::Insert);
                        }
                        tracing::debug!(%field_id, is_transfer, "edit-mode entered");
                        // Remember the last field that received user-driven
                        // focus so `i` can re-focus it on the next press.
                        self.last_focused_field = Some(field_id.clone());
                        self.edit_focus = EditFocus::Editing { field_id };
                        mode_changed = true;
                    } else if !already_editing {
                        tracing::debug!(%field_id, "focus ignored — no recent user gesture");
                    }
                }
                EditConsoleEvent::Blur { field_id } => {
                    let matches_current = match &self.edit_focus {
                        EditFocus::Editing { field_id: f } => *f == field_id,
                        EditFocus::None => false,
                    };
                    if matches_current {
                        // Defer the engine-mode flip: a Tab/Shift+Tab
                        // transfer fires Focus on a sibling field within
                        // BLUR_TRANSFER_WINDOW, in which case we stay in
                        // Insert. The expiry path in about_to_wait flips
                        // to Normal if no Focus arrives.
                        self.edit_focus = EditFocus::None;
                        self.pending_blur_at = Some(Instant::now());
                    }
                }
                EditConsoleEvent::Mutate { field_id, .. } => {
                    if let EditFocus::Editing { field_id: f } = &self.edit_focus
                        && *f == field_id
                    {
                        tracing::trace!(
                            %field_id,
                            "edit-mode: page mutation while engine attached; ignored"
                        );
                    }
                }
                EditConsoleEvent::Selection { value } => {
                    if value.is_empty() {
                        tracing::debug!("yank: selection event with empty value — nothing copied");
                    } else if let Some(host) = self.host.as_ref() {
                        let ok = host.clipboard_set_text(&value);
                        tracing::debug!(
                            len = value.len(),
                            ok,
                            "yank: selection -> system clipboard"
                        );
                    }
                }
            }
        }
        if mode_changed {
            self.refresh_title();
        }
    }

    /// Expire a pending Blur if no transferring Focus arrived within
    /// the grace window. Flips the engine to Normal at that point so a
    /// real exit from Insert (click outside an input, or a blur with
    /// no follow-up) still leaves the chrome consistent.
    fn expire_pending_blur(&mut self) {
        let Some(blurred_at) = self.pending_blur_at else {
            return;
        };
        let window = std::time::Duration::from_millis(BLUR_TRANSFER_WINDOW_MS);
        if blurred_at.elapsed() < window {
            return;
        }
        self.pending_blur_at = None;
        // Only flip to Normal if no other path already advanced the
        // engine (e.g. the Esc handler ran in the same window).
        let still_insert = self
            .engine
            .lock()
            .map(|e| matches!(e.mode(), PageMode::Insert))
            .unwrap_or(false);
        if still_insert {
            if let Ok(mut e) = self.engine.lock() {
                e.set_mode(buffr_modal::PageMode::Normal);
            }
            self.refresh_title();
        }
    }

    /// Convert a winit `KeyEvent` into a `PlannedInput` for the hjkl engine.
    ///
    /// Mirrors `buffr_modal::winit_adapter::key_event_to_chord` but targets
    /// `hjkl_engine::PlannedInput` rather than our internal `KeyChord`.
    fn winit_key_to_planned(
        event: &winit::event::KeyEvent,
        modifiers: winit::keyboard::ModifiersState,
    ) -> Option<PlannedInput> {
        use winit::event::ElementState;
        use winit::keyboard::{Key as WKey, NamedKey as WNamed};

        if event.state != ElementState::Pressed {
            return None;
        }
        let mods = EngineModifiers {
            ctrl: modifiers.control_key(),
            shift: modifiers.shift_key(),
            alt: modifiers.alt_key(),
            super_: modifiers.super_key(),
        };
        match &event.logical_key {
            WKey::Character(s) => {
                let mut chars = s.chars();
                let first = chars.next()?;
                if chars.next().is_some() {
                    return None;
                }
                Some(PlannedInput::Char(first, mods))
            }
            WKey::Named(named) => {
                let sk = match named {
                    WNamed::Escape => SpecialKey::Esc,
                    WNamed::Enter => SpecialKey::Enter,
                    WNamed::Backspace => SpecialKey::Backspace,
                    WNamed::Tab => SpecialKey::Tab,
                    WNamed::ArrowUp => SpecialKey::Up,
                    WNamed::ArrowDown => SpecialKey::Down,
                    WNamed::ArrowLeft => SpecialKey::Left,
                    WNamed::ArrowRight => SpecialKey::Right,
                    WNamed::Home => SpecialKey::Home,
                    WNamed::End => SpecialKey::End,
                    WNamed::PageUp => SpecialKey::PageUp,
                    WNamed::PageDown => SpecialKey::PageDown,
                    WNamed::Insert => SpecialKey::Insert,
                    WNamed::Delete => SpecialKey::Delete,
                    WNamed::F1 => SpecialKey::F(1),
                    WNamed::F2 => SpecialKey::F(2),
                    WNamed::F3 => SpecialKey::F(3),
                    WNamed::F4 => SpecialKey::F(4),
                    WNamed::F5 => SpecialKey::F(5),
                    WNamed::F6 => SpecialKey::F(6),
                    WNamed::F7 => SpecialKey::F(7),
                    WNamed::F8 => SpecialKey::F(8),
                    WNamed::F9 => SpecialKey::F(9),
                    WNamed::F10 => SpecialKey::F(10),
                    WNamed::F11 => SpecialKey::F(11),
                    WNamed::F12 => SpecialKey::F(12),
                    _ => return None,
                };
                Some(PlannedInput::Key(sk, mods))
            }
            _ => None,
        }
    }

    /// Handle a key event while in `Editing` state. Returns `true` if
    /// the event was consumed (the caller must not forward it further).
    ///
    /// Insert mode is "transparent" — every key is forwarded straight to
    /// CEF so the focused input field handles input natively (typing,
    /// arrow keys, selection, copy/paste, IME, etc.). The only key
    /// intercepted is `Esc`, which exits Insert mode and returns to
    /// Normal page mode.
    fn edit_mode_handle_key(&mut self, event: &winit::event::KeyEvent) -> bool {
        let planned = Self::winit_key_to_planned(event, self.modifiers);
        let is_esc_pressed = matches!(planned, Some(PlannedInput::Key(SpecialKey::Esc, _)));
        tracing::debug!(
            state = ?event.state,
            logical = ?event.logical_key,
            is_esc_pressed,
            "edit_mode_handle_key"
        );

        let EditFocus::Editing { field_id, .. } = &self.edit_focus else {
            return false;
        };

        if is_esc_pressed {
            let fid = field_id.clone();
            self.edit_focus = EditFocus::None;
            if let Some(host) = self.host.as_ref() {
                host.run_edit_detach(&fid);
                // Blur the field so further typing doesn't go to it.
                host.run_js(buffr_core::scripts::EXIT_INSERT);
            }
            if let Ok(mut e) = self.engine.lock() {
                e.set_mode(PageMode::Normal);
            }
            self.refresh_title();
            self.request_redraw();
            tracing::info!("edit_mode: exited via Esc — engine=Normal, edit_focus=None");
            return true;
        }

        // Tab / Shift+Tab in Insert mode cycles among VISIBLE inputs
        // only. The browser's native Tab handler also lands on links
        // and buttons; routing through `__buffrCycleInput` keeps focus
        // inside the editable set.
        if event.state == winit::event::ElementState::Pressed
            && matches!(planned, Some(PlannedInput::Key(SpecialKey::Tab, _)))
        {
            if let Some(host) = self.host.as_ref() {
                host.run_edit_cycle(!self.modifiers.shift_key());
            }
            return true;
        }

        // Conventional-browser tab shortcuts that the user expects to
        // work even while typing in an input: `<C-t>`, `<C-S-t>`,
        // `<C-w>`. Dispatch the matching PageAction directly so the
        // user doesn't have to leave Insert first.
        if event.state == winit::event::ElementState::Pressed
            && self.modifiers.control_key()
            && let Some(PlannedInput::Char(c, _)) = planned
        {
            let lower = c.to_ascii_lowercase();
            let action = match (lower, self.modifiers.shift_key()) {
                ('t', false) => Some(buffr_modal::PageAction::TabNewRight),
                ('t', true) => Some(buffr_modal::PageAction::ReopenClosedTab),
                ('w', false) => Some(buffr_modal::PageAction::TabClose),
                _ => None,
            };
            if let Some(a) = action {
                self.dispatch_action(&a);
                return true;
            }

            // Ctrl+V paste: CEF on Wayland can't read the system
            // clipboard itself, so we read via hjkl-clipboard and inject
            // the text into the focused element via execCommand. Done
            // here in edit_mode (not overlay/page) because the focused
            // element model is CEF's, not ours.
            if lower == 'v' && !self.modifiers.shift_key() {
                if let Some(host) = self.host.as_ref()
                    && let Some(text) = host.clipboard_text()
                    && !text.is_empty()
                {
                    let json = serde_json::to_string(&text).unwrap_or_else(|_| "\"\"".to_string());
                    let js = format!(
                        "(function(){{var t={};\
                         var el=document.activeElement;\
                         if(!el)return;\
                         try{{document.execCommand('insertText',false,t);}}\
                         catch(e){{}}\
                         }})();",
                        json
                    );
                    host.run_js(&js);
                }
                return true;
            }
        }

        // Forward every other key directly to CEF. The page handles it
        // natively — no Rust-side editor model.
        if let Some(host) = self.host.as_ref() {
            let mods = winit_mods_to_cef(&self.modifiers);
            for ev in winit_key_to_cef_events(event, mods) {
                host.osr_key_event(ev);
            }
        }
        true
    }

    /// Pull the front of the permissions queue into a renderable
    /// [`PermissionsPrompt`] if no prompt is currently shown. Returns
    /// `true` when the prompt state changed (so the caller knows to
    /// resync the CEF rect + redraw).
    fn sync_permissions_prompt(&mut self) -> bool {
        // Already showing a prompt — nothing to do until the user
        // resolves it.
        if self.permissions_prompt.is_some() {
            return false;
        }
        let queue_total = permissions_queue_len(&self.permissions_queue);
        if queue_total == 0 {
            return false;
        }
        // queue_total includes the front entry; "more pending after
        // this one" is queue_total - 1.
        let queue_after = queue_total.saturating_sub(1) as u32;
        let Some((origin, caps)) = peek_permission_front(&self.permissions_queue) else {
            return false;
        };
        let labels: Vec<String> = caps.iter().map(|c| c.human_label()).collect();
        self.permissions_prompt = Some(PermissionsPrompt {
            origin,
            capabilities: labels,
            queue_len: queue_after,
        });
        self.mark_chrome_dirty();
        true
    }

    /// Resolve the front-of-queue permission with `outcome`. The
    /// callback fires exactly once; the next prompt (if any) is
    /// drawn on the following tick via [`Self::sync_permissions_prompt`].
    fn resolve_permission(&mut self, outcome: PromptOutcome) {
        let Some(pending) = pop_permission_front(&self.permissions_queue) else {
            warn!("permissions: resolve called with empty queue");
            self.permissions_prompt = None;
            self.mark_chrome_dirty();
            return;
        };
        if let Err(err) = pending.resolve(outcome, &self.permissions) {
            warn!(error = %err, "permissions: resolve failed");
        }
        self.permissions_prompt = None;
        // Pull the next prompt immediately so the chrome shows it
        // without waiting for the next tick.
        self.sync_permissions_prompt();
        self.mark_chrome_dirty();
        self.request_redraw();
    }

    /// Resolve the close-pinned confirmation from a keypress. Returns
    /// `true` when the keypress is consumed (any key, since the prompt
    /// is modal). `y` / `<Enter>` confirms, `n` / `<Esc>` dismisses,
    /// everything else is swallowed without changing state so a stray
    /// keypress can't accidentally close the tab.
    fn confirm_handle_key(&mut self, event: &winit::event::KeyEvent) -> bool {
        if self.confirm_close_pinned.is_none() {
            return false;
        }
        if event.state != winit::event::ElementState::Pressed {
            return true;
        }
        use winit::keyboard::{Key as WKey, NamedKey as WNamed};
        match &event.logical_key {
            WKey::Character(s) => {
                let c = s.chars().next().unwrap_or('\0').to_ascii_lowercase();
                match c {
                    'y' => self.resolve_pinned_close(true),
                    'n' => self.resolve_pinned_close(false),
                    _ => {}
                }
            }
            WKey::Named(WNamed::Enter) => self.resolve_pinned_close(true),
            WKey::Named(WNamed::Escape) => self.resolve_pinned_close(false),
            _ => {}
        }
        true
    }

    /// Route a keystroke to the active permission prompt. Returns
    /// `true` when the key was consumed.
    ///
    /// Bindings: `a`/`y` allow once, `A`/`Y` allow + remember, `d`/`n`
    /// deny once, `D`/`N` deny + remember, `s` deny + remember
    /// (qutebrowser parity for "stop"), `Esc` defer.
    fn permissions_handle_key(&mut self, event: &winit::event::KeyEvent) -> bool {
        if self.permissions_prompt.is_none() {
            return false;
        }
        let chord = match key_event_to_chord(event, self.modifiers) {
            Some(c) => c,
            None => return true,
        };
        // Modifier-bearing chords (Ctrl-*, Alt-*) are swallowed so the
        // modal trie can't fire on `<C-w>c` mid-prompt.
        let mods = chord.modifiers;
        let plain = !mods.contains(buffr_modal::Modifiers::CTRL)
            && !mods.contains(buffr_modal::Modifiers::ALT)
            && !mods.contains(buffr_modal::Modifiers::SUPER);
        match chord.key {
            Key::Named(NamedKey::Esc) => {
                self.resolve_permission(PromptOutcome::Defer);
            }
            Key::Char(c) if plain => match c {
                'a' | 'y' => self.resolve_permission(PromptOutcome::Allow { remember: false }),
                'A' | 'Y' => self.resolve_permission(PromptOutcome::Allow { remember: true }),
                'd' | 'n' => self.resolve_permission(PromptOutcome::Deny { remember: false }),
                'D' | 'N' | 's' => self.resolve_permission(PromptOutcome::Deny { remember: true }),
                _ => {
                    // Unmapped — swallow so the modal engine doesn't see it.
                }
            },
            _ => {}
        }
        true
    }

    fn dispatch_omnibar(&mut self, bar: &InputBar) {
        let raw = bar.current_value().to_string();
        if raw.is_empty() {
            return;
        }
        // If a suggestion is selected its `value` is already a real
        // URL; otherwise resolve the typed buffer.
        let target = if bar.selected.is_some() {
            raw.clone()
        } else {
            // Phase 6 telemetry: count one search when the resolver
            // would fall through to the search-engine template.
            // Selecting a history/bookmark suggestion does NOT count
            // as a search — those are direct navigations.
            if buffr_config::classify_input(&raw) == buffr_config::InputKind::Search {
                self.counters.increment(buffr_core::KEY_SEARCHES_RUN);
            }
            buffr_config::resolve_input(&raw, &self.search_config)
        };
        if target.is_empty() {
            return;
        }
        if let Some(host) = self.host.as_ref()
            && let Err(err) = host.navigate(&target)
        {
            warn!(error = %err, target = %target, "omnibar: navigate failed");
        }
    }

    fn dispatch_find(&mut self, bar: &InputBar, forward: bool) {
        let query = bar.current_value().trim().to_string();
        if query.is_empty() {
            return;
        }
        self.statusline.find_query = Some(FindStatus {
            query: query.clone(),
            current: 0,
            total: 0,
        });
        if let Some(host) = self.host.as_ref() {
            host.start_find(&query, forward);
        }
    }

    /// Paint one popup window frame: a minimal address bar + OSR content.
    fn paint_popup_window(&mut self, window_id: WindowId) {
        let popup = match self.popups.get_mut(&window_id) {
            Some(p) => p,
            None => return,
        };
        let inner = popup.window.inner_size();
        let width = inner.width.max(1);
        let height = inner.height.max(1);
        let bar_h = STATUSLINE_HEIGHT;

        let osr_meta: Option<(u32, u32, u64)> = if let Ok(mut frame) = popup.frame.lock() {
            if frame.width > 0 && frame.height > 0 && !frame.pixels.is_empty() {
                std::mem::swap(&mut popup.osr_scratch, &mut frame.pixels);
                Some((frame.width, frame.height, frame.generation))
            } else {
                None
            }
        } else {
            None
        };

        let chrome_dirty = popup.chrome_generation != popup.last_painted_chrome_gen;
        popup.renderer.resize(width, height);
        let url = popup.url.clone();
        let new_gen;
        let res = if let Some((osr_w, osr_h, osr_gen)) = osr_meta {
            new_gen = osr_gen;
            let osr_upload = crate::render::OsrUpload {
                pixels: &popup.osr_scratch,
                width: osr_w,
                height: osr_h,
                generation: osr_gen,
                dst_rect: (0, bar_h, width, height.saturating_sub(bar_h).max(1)),
            };
            popup.renderer.frame(
                chrome_dirty,
                |buf, w, h| paint_popup_chrome(buf, w, h, &url, bar_h),
                Some(osr_upload),
            )
        } else {
            new_gen = popup.last_osr_generation;
            popup.renderer.frame(
                chrome_dirty,
                |buf, w, h| paint_popup_chrome(buf, w, h, &url, bar_h),
                None,
            )
        };

        popup.last_osr_generation = new_gen;
        if chrome_dirty {
            popup.last_painted_chrome_gen = popup.chrome_generation;
        }
        if let Err(err) = res {
            warn!(error = %err, "popup: wgpu frame failed");
        }
    }

    /// Handle a `WindowEvent` for a popup window.
    fn handle_popup_window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        let browser_id = self
            .popups
            .get(&window_id)
            .map(|p| p.browser_id)
            .unwrap_or(-1);

        match event {
            WindowEvent::CloseRequested => {
                debug!(browser_id, "popup: CloseRequested");
                if let Some(host) = self.host.as_ref()
                    && browser_id >= 0
                {
                    host.popup_close(browser_id);
                }
                // Remove immediately; CEF on_before_close also drains
                // popup_close_sink on the next about_to_wait tick.
                self.popup_window_id_by_browser.remove(&browser_id);
                self.popups.remove(&window_id);
            }
            WindowEvent::RedrawRequested => {
                self.paint_popup_window(window_id);
            }
            WindowEvent::Resized(new_size) => {
                if browser_id >= 0 {
                    let w = new_size.width.max(1);
                    let h = new_size.height.max(1);
                    if let Some(host) = self.host.as_ref() {
                        host.popup_resize(browser_id, w, h);
                    }
                    if let Some(popup) = self.popups.get_mut(&window_id) {
                        popup.chrome_generation = popup.chrome_generation.wrapping_add(1);
                    }
                }
                self.paint_popup_window(window_id);
            }
            WindowEvent::ModifiersChanged(mods) => {
                if let Some(popup) = self.popups.get_mut(&window_id) {
                    popup.modifiers = mods.state();
                }
            }
            WindowEvent::Focused(focused) => {
                if let Some(host) = self.host.as_ref()
                    && browser_id >= 0
                {
                    host.popup_osr_focus(browser_id, focused);
                }
            }
            WindowEvent::CursorLeft { .. } => {
                let mods = self
                    .popups
                    .get(&window_id)
                    .map(|p| winit_mods_to_cef(&p.modifiers))
                    .unwrap_or(0);
                if let Some(host) = self.host.as_ref()
                    && browser_id >= 0
                {
                    // Simulate mouse leave by moving to (0,0) outside the
                    // browser rect — same pattern as main window CursorLeft.
                    host.popup_osr_mouse_move(browser_id, 0, 0, mods);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let Some(popup) = self.popups.get_mut(&window_id) else {
                    return;
                };
                let bar_h = STATUSLINE_HEIGHT as i32;
                let bx = position.x as i32;
                // Cursor y relative to the content area (below address bar).
                let by = (position.y as i32).saturating_sub(bar_h);
                popup.cursor = (bx, by);
                let mods = winit_mods_to_cef(&popup.modifiers) | popup.mouse_buttons;
                if let Some(host) = self.host.as_ref()
                    && browser_id >= 0
                {
                    host.popup_osr_mouse_move(browser_id, bx, by, mods);
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                use winit::event::ElementState::Pressed;
                let Some(popup) = self.popups.get_mut(&window_id) else {
                    return;
                };
                let Some(cef_button) = winit_button_to_cef(&button) else {
                    return;
                };
                let mouse_up = state != Pressed;
                let btn_flag: u32 = if cef_button == MouseButtonType::LEFT {
                    16
                } else if cef_button == MouseButtonType::MIDDLE {
                    32
                } else {
                    64
                };
                if mouse_up {
                    popup.mouse_buttons &= !btn_flag;
                } else {
                    popup.mouse_buttons |= btn_flag;
                }
                let now = Instant::now();
                if !mouse_up {
                    let same = popup
                        .last_click_button
                        .map(|b| b == cef_button)
                        .unwrap_or(false);
                    if same && now.duration_since(popup.last_click_at) < DOUBLE_CLICK_WINDOW {
                        popup.click_count = (popup.click_count + 1).min(3);
                    } else {
                        popup.click_count = 1;
                    }
                    popup.last_click_at = now;
                    popup.last_click_button = Some(cef_button);
                }
                let (bx, by) = popup.cursor;
                let mods = winit_mods_to_cef(&popup.modifiers) | popup.mouse_buttons;
                let click_count = popup.click_count;
                let in_content = by >= 0;
                if let Some(host) = self.host.as_ref()
                    && browser_id >= 0
                {
                    // Pressed inside the OSR content (below the address bar)
                    // → focus the popup browser so DOM clicks deliver focus
                    // to inputs and keystrokes route to this popup. Wayland
                    // doesn't reliably emit Focused() on click, so we drive
                    // it explicitly.
                    if !mouse_up && in_content {
                        host.popup_osr_focus(browser_id, true);
                    }
                    host.popup_osr_mouse_click(
                        browser_id,
                        bx,
                        by,
                        cef_button,
                        mouse_up,
                        click_count,
                        mods,
                    );
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                use winit::event::MouseScrollDelta;
                // Two-finger horizontal-swipe back/forward — same path
                // as the main window, routed to the popup's own history.
                if let MouseScrollDelta::PixelDelta(px) = delta {
                    if let Some(action) = self.detect_swipe(px.x as f32, px.y as f32) {
                        if let Some(host) = self.host.as_ref()
                            && browser_id >= 0
                        {
                            match action {
                                buffr_modal::PageAction::HistoryBack => {
                                    host.popup_history_back(browser_id);
                                }
                                buffr_modal::PageAction::HistoryForward => {
                                    host.popup_history_forward(browser_id);
                                }
                                _ => {}
                            }
                        }
                        return;
                    }
                    if self.swipe_committed {
                        return;
                    }
                }

                let Some(popup) = self.popups.get(&window_id) else {
                    return;
                };
                let (bx, by) = popup.cursor;
                let mods = winit_mods_to_cef(&popup.modifiers);
                let (dx, dy, _is_pixel) = winit_wheel_to_cef_delta(&delta);
                if let Some(host) = self.host.as_ref()
                    && browser_id >= 0
                {
                    host.popup_osr_mouse_wheel(browser_id, bx, by, dx, dy, mods);
                }
            }
            WindowEvent::KeyboardInput { event: key_ev, .. } => {
                let Some(popup) = self.popups.get(&window_id) else {
                    return;
                };
                let mods = winit_mods_to_cef(&popup.modifiers);
                let events = winit_key_to_cef_events(&key_ev, mods);
                if let Some(host) = self.host.as_ref()
                    && browser_id >= 0
                {
                    for ev in events {
                        host.popup_osr_key_event(browser_id, ev);
                    }
                }
            }
            _ => {}
        }
    }
}

// ---- OSR input helpers ---------------------------------------------------

/// Convert a winit `MouseScrollDelta` to a CEF wheel delta (dx, dy, is_pixel).
///
/// CEF's `send_mouse_wheel_event` takes integer deltas in wheel-tick units
/// (~120 = 1 line). winit's `PixelDelta` is raw px per event (~4-6 px on
/// touchpads / high-res wheels), which CEF rounds to near-zero on its own,
/// so we scale by `PIXEL_DELTA_SCALE` (10× — empirical sweet spot for
/// touchpad feel after testing).
fn winit_wheel_to_cef_delta(delta: &winit::event::MouseScrollDelta) -> (i32, i32, bool) {
    use winit::event::MouseScrollDelta;
    const PIXEL_DELTA_SCALE: f32 = 10.0;
    match delta {
        MouseScrollDelta::LineDelta(x, y) => ((x * 120.0) as i32, (y * 120.0) as i32, false),
        MouseScrollDelta::PixelDelta(p) => (
            (p.x as f32 * PIXEL_DELTA_SCALE) as i32,
            (p.y as f32 * PIXEL_DELTA_SCALE) as i32,
            true,
        ),
    }
}

/// Convert winit `ModifiersState` to CEF event-flag bits.
///
/// CEF bit values (from cef_dll_sys `cef_event_flags_t`):
///   SHIFT   = 2
///   CONTROL = 4
///   ALT     = 8
///   COMMAND = 128
fn winit_mods_to_cef(m: &ModifiersState) -> u32 {
    let mut flags: u32 = 0;
    if m.shift_key() {
        flags |= 2;
    }
    if m.control_key() {
        flags |= 4;
    }
    if m.alt_key() {
        flags |= 8;
    }
    if m.super_key() {
        flags |= 128;
    }
    flags
}

/// Map a winit `PhysicalKey` to a Windows virtual-key code for CEF.
///
/// Coverage: A-Z, 0-9, F1-F12, common navigation and editing keys.
/// Unknowns map to 0 (CEF ignores `windows_key_code == 0` for non-printable
/// keys; printable keys use `character` instead).
fn physical_key_to_vk(key: &winit::keyboard::PhysicalKey) -> i32 {
    use winit::keyboard::{KeyCode, PhysicalKey};
    let PhysicalKey::Code(code) = key else {
        return 0;
    };
    match code {
        KeyCode::KeyA => 0x41,
        KeyCode::KeyB => 0x42,
        KeyCode::KeyC => 0x43,
        KeyCode::KeyD => 0x44,
        KeyCode::KeyE => 0x45,
        KeyCode::KeyF => 0x46,
        KeyCode::KeyG => 0x47,
        KeyCode::KeyH => 0x48,
        KeyCode::KeyI => 0x49,
        KeyCode::KeyJ => 0x4A,
        KeyCode::KeyK => 0x4B,
        KeyCode::KeyL => 0x4C,
        KeyCode::KeyM => 0x4D,
        KeyCode::KeyN => 0x4E,
        KeyCode::KeyO => 0x4F,
        KeyCode::KeyP => 0x50,
        KeyCode::KeyQ => 0x51,
        KeyCode::KeyR => 0x52,
        KeyCode::KeyS => 0x53,
        KeyCode::KeyT => 0x54,
        KeyCode::KeyU => 0x55,
        KeyCode::KeyV => 0x56,
        KeyCode::KeyW => 0x57,
        KeyCode::KeyX => 0x58,
        KeyCode::KeyY => 0x59,
        KeyCode::KeyZ => 0x5A,
        KeyCode::Digit0 => 0x30,
        KeyCode::Digit1 => 0x31,
        KeyCode::Digit2 => 0x32,
        KeyCode::Digit3 => 0x33,
        KeyCode::Digit4 => 0x34,
        KeyCode::Digit5 => 0x35,
        KeyCode::Digit6 => 0x36,
        KeyCode::Digit7 => 0x37,
        KeyCode::Digit8 => 0x38,
        KeyCode::Digit9 => 0x39,
        KeyCode::F1 => 0x70,
        KeyCode::F2 => 0x71,
        KeyCode::F3 => 0x72,
        KeyCode::F4 => 0x73,
        KeyCode::F5 => 0x74,
        KeyCode::F6 => 0x75,
        KeyCode::F7 => 0x76,
        KeyCode::F8 => 0x77,
        KeyCode::F9 => 0x78,
        KeyCode::F10 => 0x79,
        KeyCode::F11 => 0x7A,
        KeyCode::F12 => 0x7B,
        KeyCode::ArrowLeft => 0x25,
        KeyCode::ArrowUp => 0x26,
        KeyCode::ArrowRight => 0x27,
        KeyCode::ArrowDown => 0x28,
        KeyCode::Enter => 0x0D,
        KeyCode::Backspace => 0x08,
        KeyCode::Delete => 0x2E,
        KeyCode::Tab => 0x09,
        KeyCode::Escape => 0x1B,
        KeyCode::Space => 0x20,
        KeyCode::Home => 0x24,
        KeyCode::End => 0x23,
        KeyCode::PageUp => 0x21,
        KeyCode::PageDown => 0x22,
        KeyCode::Insert => 0x2D,
        _ => 0,
    }
}

/// Build a CEF `KeyEvent` from a winit keyboard event.
///
/// Returns `None` for modifier-only presses (no VK code, no character).
fn winit_key_to_cef_events(event: &winit::event::KeyEvent, modifiers: u32) -> Vec<KeyEvent> {
    use winit::event::ElementState;

    let vk = physical_key_to_vk(&event.physical_key);
    let ch: u16 = event
        .text
        .as_ref()
        .and_then(|t| t.chars().next())
        .map(|c| {
            let mut buf = [0u16; 2];
            let encoded = c.encode_utf16(&mut buf);
            if encoded.len() == 1 { encoded[0] } else { 0 }
        })
        .unwrap_or(0);

    // Skip pure modifier keys (no VK, no character text).
    if vk == 0 && ch == 0 {
        return Vec::new();
    }

    match event.state {
        ElementState::Pressed => {
            let raw = KeyEvent {
                type_: KeyEventType::RAWKEYDOWN,
                modifiers,
                windows_key_code: vk,
                native_key_code: 0,
                is_system_key: 0,
                character: ch,
                unmodified_character: ch,
                focus_on_editable_field: 0,
                ..KeyEvent::default()
            };
            if ch != 0 {
                let char_ev = KeyEvent {
                    type_: KeyEventType::CHAR,
                    modifiers,
                    windows_key_code: ch as i32,
                    native_key_code: 0,
                    is_system_key: 0,
                    character: ch,
                    unmodified_character: ch,
                    focus_on_editable_field: 0,
                    ..KeyEvent::default()
                };
                vec![raw, char_ev]
            } else {
                vec![raw]
            }
        }
        ElementState::Released => {
            vec![KeyEvent {
                type_: KeyEventType::KEYUP,
                modifiers,
                windows_key_code: vk,
                native_key_code: 0,
                is_system_key: 0,
                character: ch,
                unmodified_character: ch,
                focus_on_editable_field: 0,
                ..KeyEvent::default()
            }]
        }
    }
}

/// Paint a minimal popup chrome: one address-bar strip at the top.
fn paint_popup_chrome(buf: &mut [u32], w: usize, h: usize, url: &str, bar_h: u32) {
    use buffr_ui::font;
    let bar_h = bar_h as usize;
    if w == 0 || h < bar_h {
        return;
    }
    // Background — same shade as the Normal statusline.
    let bg: u32 = 0xFF_16_30_18;
    let fg: u32 = 0xFF_EE_EE_EE;
    for row in 0..bar_h {
        let start = row * w;
        let end = (start + w).min(buf.len());
        if let Some(slice) = buf.get_mut(start..end) {
            for pixel in slice {
                *pixel = bg;
            }
        }
    }
    // URL text, left-padded by 8 px, vertically centred in bar_h.
    let text_y = ((bar_h as i32 - font::glyph_h() as i32) / 2).max(0) as i32;
    font::draw_text(buf, w, h, 8, text_y, url, fg);
}

/// Omnibar / command-line popup geometry.
const OMNIBAR_POPUP_MAX_WIDTH: u32 = 800;
const OMNIBAR_POPUP_BORDER: u32 = 2;
const OMNIBAR_POPUP_BG: u32 = 0xFF_1A_1B_26;
const OMNIBAR_POPUP_BORDER_COLOR: u32 = 0xFF_7A_A2_F7;

/// Fill a rectangle in a u32 pixel buffer with stride `buf_w`.
#[allow(clippy::too_many_arguments)]
fn fill_rect_u32(
    buf: &mut [u32],
    buf_w: usize,
    buf_h: usize,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    color: u32,
) {
    let x1 = (x + w).min(buf_w);
    let y1 = (y + h).min(buf_h);
    if x >= x1 || y >= y1 {
        return;
    }
    for row in y..y1 {
        let base = row * buf_w;
        let row_end = base + buf_w;
        if row_end > buf.len() {
            break;
        }
        buf[base + x..base + x1].fill(color);
    }
}

/// Paint only the chrome strips (statusline, tab strip, popups, download
/// notice, overlay) into `buf`. The CEF region rows are never touched —
/// they must remain at `0x00_00_00_00` (transparent) so the OSR texture
/// shows through the alpha-blended chrome layer.
///
/// All colours written here use `0xFF_RR_GG_BB` (fully-opaque BGRA) so
/// the GPU alpha-blend composite produces crisp chrome-over-OSR output.
#[allow(clippy::too_many_arguments)]
fn paint_chrome_strips(
    buf: &mut [u32],
    w: usize,
    height: u32,
    statusline: &Statusline,
    tab_strip: &TabStrip,
    tab_y: u32,
    notice_y: u32,
    current_notice: Option<&buffr_core::DownloadNotice>,
    confirm_close_pinned: Option<buffr_core::TabId>,
    permissions_prompt: Option<&PermissionsPrompt>,
    overlay_data: Option<&InputBar>,
) {
    let h = height as usize;

    // Statusline — bottom strip.
    statusline.paint(buf, w, h);

    // Tab strip — between download notice and CEF region.
    tab_strip.paint(buf, w, h, tab_y);

    // Permissions prompt OR pinned-close confirmation.
    let win_w = w as u32;
    let has_prompt = confirm_close_pinned.is_some() || permissions_prompt.is_some();
    if has_prompt {
        let popup_w = ((win_w * 60) / 100).clamp(300, OMNIBAR_POPUP_MAX_WIDTH);
        let popup_x = (win_w - popup_w) / 2;
        let popup_y = height / 3;
        let content_h = buffr_ui::CONFIRM_PROMPT_HEIGHT;
        let popup_h = (content_h + 2 * OMNIBAR_POPUP_BORDER).min(height.saturating_sub(popup_y));
        fill_rect_u32(
            buf,
            w,
            h,
            popup_x as usize,
            popup_y as usize,
            popup_w as usize,
            popup_h as usize,
            OMNIBAR_POPUP_BORDER_COLOR,
        );
        let inner_x = popup_x + OMNIBAR_POPUP_BORDER;
        let inner_y = popup_y + OMNIBAR_POPUP_BORDER;
        let inner_w = popup_w.saturating_sub(2 * OMNIBAR_POPUP_BORDER);
        let inner_h = popup_h.saturating_sub(2 * OMNIBAR_POPUP_BORDER);
        fill_rect_u32(
            buf,
            w,
            h,
            inner_x as usize,
            inner_y as usize,
            inner_w as usize,
            inner_h as usize,
            OMNIBAR_POPUP_BG,
        );
        if confirm_close_pinned.is_some() {
            let confirm_widget = buffr_ui::ConfirmPrompt {
                message: "Close pinned tab?".to_string(),
                yes_label: "Yes (y)".to_string(),
                no_label: "No (n)".to_string(),
            };
            confirm_widget.paint_at(buf, w, h, inner_x, inner_y, inner_w);
        } else if let Some(prompt) = permissions_prompt {
            prompt.paint_at(buf, w, h, inner_x, inner_y, inner_w);
        }
    }

    // Download notice strip.
    if let Some(notice) = current_notice {
        let strip = DownloadNoticeStrip {
            kind: match notice.kind {
                buffr_core::DownloadNoticeKind::Started => DownloadNoticeKind::Started,
                buffr_core::DownloadNoticeKind::Completed => DownloadNoticeKind::Completed,
                buffr_core::DownloadNoticeKind::Failed => DownloadNoticeKind::Failed,
            },
            filename: notice.filename.clone(),
            path: notice.path.clone(),
        };
        strip.paint(buf, w, h, notice_y);
    }

    // Overlay popup (omnibar / command / find).
    if let Some(bar) = overlay_data {
        let popup_w = ((win_w * 60) / 100).clamp(200, OMNIBAR_POPUP_MAX_WIDTH);
        let popup_x = (win_w - popup_w) / 2;
        let popup_y = height / 3;
        let popup_h =
            (bar.total_height() + 2 * OMNIBAR_POPUP_BORDER).min(height.saturating_sub(popup_y));
        fill_rect_u32(
            buf,
            w,
            h,
            popup_x as usize,
            popup_y as usize,
            popup_w as usize,
            popup_h as usize,
            OMNIBAR_POPUP_BORDER_COLOR,
        );
        let inner_x = popup_x + OMNIBAR_POPUP_BORDER;
        let inner_y = popup_y + OMNIBAR_POPUP_BORDER;
        let inner_w = popup_w.saturating_sub(2 * OMNIBAR_POPUP_BORDER);
        let inner_h = popup_h.saturating_sub(2 * OMNIBAR_POPUP_BORDER);
        fill_rect_u32(
            buf,
            w,
            h,
            inner_x as usize,
            inner_y as usize,
            inner_w as usize,
            inner_h as usize,
            OMNIBAR_POPUP_BG,
        );
        bar.paint_at(
            buf,
            w,
            h,
            inner_x as usize,
            inner_y as usize,
            inner_w as usize,
            inner_h as usize,
        );
    }
}

/// Double-click detection window.
const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(500);

/// Map a winit `MouseButton` to a CEF `MouseButtonType`.
/// Returns `None` for `Other(_)` buttons.
fn winit_button_to_cef(button: &winit::event::MouseButton) -> Option<MouseButtonType> {
    use winit::event::MouseButton;
    match button {
        MouseButton::Left => Some(MouseButtonType::LEFT),
        MouseButton::Right => Some(MouseButtonType::RIGHT),
        MouseButton::Middle => Some(MouseButtonType::MIDDLE),
        _ => None,
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
        PageMode::Insert => "INSERT",
    }
}

impl ApplicationHandler<BuffrUserEvent> for AppState {
    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: BuffrUserEvent) {
        match event {
            BuffrUserEvent::OsrFrame => {
                tracing::trace!("user_event: OsrFrame -> request_redraw");
                self.request_redraw();
            }
            BuffrUserEvent::OsrFramePopup(browser_id) => {
                tracing::trace!(browser_id, "user_event: OsrFramePopup -> request_redraw");
                if let Some(&wid) = self.popup_window_id_by_browser.get(&browser_id)
                    && let Some(popup) = self.popups.get(&wid)
                {
                    popup.window.request_redraw();
                }
            }
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let win_attrs = Window::default_attributes()
            .with_title(self.title_for(self.current_mode_label, &self.statusline.url))
            .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 800.0));
        let window = match event_loop.create_window(win_attrs) {
            Ok(w) => w,
            Err(err) => {
                warn!(error = %err, "failed to create window");
                event_loop.exit();
                return;
            }
        };
        let window = Arc::new(window);

        let raw = match window.window_handle() {
            Ok(h) => h.as_raw(),
            Err(err) => {
                warn!(error = %err, "no raw window handle");
                event_loop.exit();
                return;
            }
        };

        // Pass the same page viewport used by later resize events so
        // CEF paints the first frame in the area below the tab strip and
        // above the statusline.
        let inner = window.inner_size();
        let (_cef_x, _cef_y, cef_w, cef_h) =
            self.cef_child_rect(inner.width.max(1), inner.height.max(1));

        match buffr_core::BrowserHost::new_with_options(
            raw,
            &self.homepage,
            self.history.clone(),
            self.downloads.clone(),
            self.downloads_config.clone(),
            self.zoom.clone(),
            self.permissions.clone(),
            self.permissions_queue.clone(),
            self.download_notice_queue.clone(),
            self.find_sink.clone(),
            self.hint_sink.clone(),
            self.edit_sink.clone(),
            self.hint_alphabet.clone(),
            (cef_w, cef_h),
            self.private,
            Some(self.counters.clone()),
        ) {
            Ok(host) => {
                info!(mode = ?host.mode(), "browser host created");
                debug!(url = %self.homepage, "browser host created — initial url");
                // CEF stays focused for the lifetime of the browser
                // so DOM clicks deliver focus to inputs. We do NOT
                // forward OS-level Focused(false) (alt-tab) so pages
                // retain state. Insert mode transitions are tracked
                // independently via the modal engine.
                host.osr_focus(true);
                // Store the popup event sinks so `about_to_wait` can drain them.
                self.popup_create_sink = host.popup_create_sink();
                self.popup_close_sink = host.popup_close_sink();
                // Wire OSR on_paint → winit redraw via EventLoopProxy.
                // Wayland's frame-callback model means request_redraw on
                // an idle surface never fires; this wakeup is what
                // delivers freshly-painted CEF frames to softbuffer.
                let proxy = self.event_proxy.clone();
                host.set_osr_wake(Arc::new(move || {
                    let _ = proxy.send_event(BuffrUserEvent::OsrFrame);
                }));
                // Match CEF's OSR frame rate to the display refresh
                // rate so scrolling / video / animations don't stutter
                // at CEF's 30 fps default. CEF clamps internally
                // (147.x caps at 60). Falls back to 60 when the
                // monitor doesn't report a rate.
                let display_hz = window
                    .current_monitor()
                    .and_then(|m| m.refresh_rate_millihertz())
                    .map(|mhz| (mhz / 1000).max(1))
                    .unwrap_or(60);
                host.set_frame_rate(display_hz);
                tracing::debug!(display_hz, "OSR frame rate set");
                self.host = Some(host);
            }
            Err(err) => {
                warn!(error = %err, "failed to create browser host");
            }
        }

        // Initialise wgpu renderer. On failure, log and exit — there is
        // no CPU-only fallback in this code path.
        match crate::render::Renderer::new(window.clone()) {
            Ok(r) => self.renderer = Some(r),
            Err(err) => {
                warn!(error = %err, "wgpu renderer init failed");
                event_loop.exit();
                return;
            }
        }

        // Schedule the find smoke-test dispatch for 1.5s after window
        // creation. This is a coarse "page is probably ready" timer
        // because we don't yet hook `OnLoadEnd` into the host.
        if self.pending_find.is_some() {
            self.find_smoke_at = Some(Instant::now() + Duration::from_millis(1500));
        }

        // Restore extra tabs from session + CLI now that the host
        // exists. The first session tab (if any) replaces the
        // homepage on tab 0; the rest open in the background.
        self.open_pending_tabs();
        self.refresh_tab_strip();

        self.window = Some(window);
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        // Dispatch popup windows before the main window path.
        if self.popups.contains_key(&window_id) {
            self.handle_popup_window_event(event_loop, window_id, event);
            return;
        }
        match event {
            WindowEvent::CloseRequested => {
                info!("close requested");
                self.save_session_now();
                event_loop.exit();
            }
            WindowEvent::RedrawRequested => {
                tracing::trace!("redraw: RedrawRequested");
                self.paint_chrome();
            }
            WindowEvent::Resized(new_size) => {
                let inner = self
                    .window
                    .as_ref()
                    .map(|w| w.inner_size())
                    .unwrap_or_default();
                let scale = self
                    .window
                    .as_ref()
                    .map(|w| w.scale_factor())
                    .unwrap_or(1.0);
                let (_x, _y, cef_w, cef_h) =
                    self.cef_child_rect(new_size.width.max(1), new_size.height.max(1));
                debug!(
                    new_w = new_size.width,
                    new_h = new_size.height,
                    inner_w = inner.width,
                    inner_h = inner.height,
                    scale,
                    cef_w,
                    cef_h,
                    has_host = self.host.is_some(),
                    "winit: Resized",
                );
                if let Some(host) = self.host.as_ref() {
                    match host.mode() {
                        buffr_core::HostMode::Windowed => {
                            host.resize(cef_w, cef_h);
                        }
                        buffr_core::HostMode::Osr => {
                            // Notify CEF on every Resized. The renderer GPU-
                            // stretches whatever frame CEF most recently
                            // emitted to fill the live browser_rect, so
                            // intermediate CEF frames at any size composite
                            // correctly without throttling/debouncing.
                            host.osr_resize(cef_w, cef_h);
                            debug!(cef_w, cef_h, "winit: Resized -> osr_resize");
                        }
                    }
                }
                // Paint synchronously so the configure ack carries a
                // buffer matching this event's size. Hyprland (and other
                // wlroots compositors) anchor top-edge resize at the
                // cursor — the window bounds grow immediately and any
                // client-paint latency shows up as a letterbox at the
                // bottom of the window while the stale buffer is still
                // attached. With the GPU compositor a paint is ~1-2 ms
                // so doing it inline here is cheaper than the visible
                // lag coalescing produces.
                //
                // Pass `new_size` explicitly: `window.inner_size()` can
                // lag the event on Hyprland during a fast top-edge drag,
                // and if paint_chrome reads the stale value we present
                // a buffer smaller than the configured surface — the
                // compositor then fills the gap by replicating the
                // bottom edge of the buffer (statusline last row),
                // which reads as a "stretched" bottom bar.
                // Update the subsurface position BEFORE paint_chrome runs.
                // wl_subsurface.set_position is double-buffered against the
                // PARENT surface commit (applies on parent commit, not on
                // child commit, even in desync mode). paint_chrome below
                // commits the parent via wgpu present, so set_size must
                // queue the new position into the parent's pending state
                // first — otherwise the position update is one frame
                // behind and the subsurface tracks the previous resize.
                // paint_chrome_with calls sub.set_size internally before
                // the wgpu present, keeping the subsurface position synced
                // with the parent commit's buffer dims. Don't duplicate the
                // call here — both paths would do the same work.
                let w = new_size.width.max(1);
                let h = new_size.height.max(1);
                self.mark_chrome_dirty();
                self.paint_chrome_with(Some((w, h)));
            }
            WindowEvent::Moved(pos) => {
                debug!(x = pos.x, y = pos.y, "winit: Moved");
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                debug!(scale_factor, "winit: ScaleFactorChanged");
            }
            WindowEvent::ModifiersChanged(mods) => {
                self.modifiers = mods.state();
            }
            WindowEvent::Focused(_focused) => {
                // OS-level window focus is intentionally NOT forwarded
                // to CEF. CEF focus tracks buffr's modal state instead
                // (Insert = focused, Normal = unfocused). This stops
                // pages losing input state when the user alt-tabs or
                // brings up another window.
            }
            WindowEvent::CursorLeft { .. } => {
                if let Some(host) = self.host.as_ref() {
                    let mods = winit_mods_to_cef(&self.modifiers);
                    host.osr_mouse_leave(mods);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                if let Some(host) = self.host.as_ref()
                    && host.mode() == buffr_core::HostMode::Osr
                {
                    // Convert from window coords to browser-region coords.
                    // The browser region starts at `cef_y` (below the chrome strips).
                    let size = self
                        .window
                        .as_ref()
                        .map(|w| w.inner_size())
                        .unwrap_or_default();
                    let (_cx, cef_y, _cw, _ch) =
                        self.cef_child_rect(size.width.max(1), size.height.max(1));
                    let bx = position.x as i32;
                    let by = (position.y as i32).saturating_sub(cef_y as i32);
                    self.osr_cursor = (bx, by);
                    let mods = winit_mods_to_cef(&self.modifiers) | self.osr_mouse_buttons;
                    host.osr_mouse_move(bx, by, mods);

                    // Promote to Visual the moment a left-button drag
                    // crosses the threshold — Chromium has already begun
                    // extending the page selection (see osr_mouse_buttons
                    // wiring), so the engine should reflect that without
                    // waiting for button-up.
                    if (self.osr_mouse_buttons & 16) != 0
                        && let Some((sx, sy)) = self.osr_drag_start
                    {
                        const DRAG_THRESHOLD_PX: i32 = 4;
                        let dx = (bx - sx).abs();
                        let dy = (by - sy).abs();
                        if dx > DRAG_THRESHOLD_PX || dy > DRAG_THRESHOLD_PX {
                            let already_visual = self
                                .engine
                                .lock()
                                .map(|e| e.mode() == PageMode::Visual)
                                .unwrap_or(true);
                            if !already_visual {
                                if let Ok(mut e) = self.engine.lock() {
                                    e.set_mode(PageMode::Visual);
                                }
                                self.refresh_title();
                            }
                            // Clear so MouseInput release path doesn't
                            // double-fire the Visual transition.
                            self.osr_drag_start = None;
                        }
                    }
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                use winit::event::{ElementState::Pressed, MouseButton};
                tracing::trace!(?state, ?button, cursor = ?self.osr_cursor, "input: mouse_button");
                // Pinned-close confirmation hit-test: a left click on
                // the Yes / No button resolves the prompt. Anywhere else
                // is swallowed so the click can't reach the page or
                // the tab strip while a modal banner is up.
                if state == Pressed
                    && button == MouseButton::Left
                    && self.confirm_close_pinned.is_some()
                {
                    let (px, py) = self.osr_cursor;
                    let size = self
                        .window
                        .as_ref()
                        .map(|w| w.inner_size())
                        .unwrap_or_default();
                    let win_w = size.width.max(1);
                    let win_h = size.height.max(1);
                    let abs_x = px;
                    let abs_y = py + self.cef_child_rect(win_w, win_h).1 as i32;
                    // Mirror popup geometry from the paint site exactly.
                    let popup_w = ((win_w * 60) / 100).clamp(300, OMNIBAR_POPUP_MAX_WIDTH);
                    let popup_x = (win_w - popup_w) / 2;
                    let popup_y = win_h / 3;
                    let inner_x = popup_x + OMNIBAR_POPUP_BORDER;
                    let inner_y = popup_y + OMNIBAR_POPUP_BORDER;
                    let inner_w = popup_w.saturating_sub(2 * OMNIBAR_POPUP_BORDER);
                    let confirm = buffr_ui::ConfirmPrompt {
                        message: String::new(),
                        yes_label: "Yes (y)".to_string(),
                        no_label: "No (n)".to_string(),
                    };
                    let (yes_rect, no_rect) = confirm.button_rects_at(inner_x, inner_y, inner_w);
                    if buffr_ui::rect_contains(yes_rect, abs_x, abs_y) {
                        self.resolve_pinned_close(true);
                        return;
                    }
                    if buffr_ui::rect_contains(no_rect, abs_x, abs_y) {
                        self.resolve_pinned_close(false);
                        return;
                    }
                    // Click missed the buttons — swallow the event so
                    // it doesn't fall through to tab-strip / page hit
                    // testing while the modal is open.
                    return;
                }
                // Back/Forward side buttons → history navigation regardless
                // of host mode. Intercept before OSR dispatch.
                if state == Pressed {
                    match button {
                        MouseButton::Back => {
                            self.dispatch_action(&buffr_modal::PageAction::HistoryBack);
                            return;
                        }
                        MouseButton::Forward => {
                            self.dispatch_action(&buffr_modal::PageAction::HistoryForward);
                            return;
                        }
                        _ => {}
                    }
                }

                // Tab-strip click: Left = focus / drag, Middle = close.
                // Press on left selects the tab AND records a drag src;
                // release on left finalizes the drag if the cursor moved
                // to a different slot.
                let tab_strip_idx = self.hit_test_tab_strip();
                if state == Pressed
                    && button == MouseButton::Left
                    && let Some(idx) = tab_strip_idx
                {
                    if let Some(host) = self.host.as_ref() {
                        host.select_tab(self.tab_ids[idx]);
                    }
                    self.tab_drag_src = Some(idx);
                    return;
                }
                if state != Pressed
                    && button == MouseButton::Left
                    && let Some(src) = self.tab_drag_src.take()
                    && let Some(dst) = tab_strip_idx
                    && dst != src
                    && let Some(host) = self.host.as_ref()
                {
                    host.move_tab(src, dst);
                    self.mark_session_dirty();
                    self.refresh_tab_strip();
                    self.request_redraw();
                    return;
                }
                if state == Pressed
                    && button == MouseButton::Middle
                    && let Some(idx) = tab_strip_idx
                {
                    let id = self.tab_ids[idx];
                    // Middle-click on a pinned tab also gates through
                    // the confirmation overlay so the user can't lose
                    // a pinned tab by misaiming.
                    let pinned = self
                        .host
                        .as_ref()
                        .and_then(|h| h.tabs_summary().get(idx).map(|t| t.pinned))
                        .unwrap_or(false);
                    if pinned && self.confirm_close_pinned.is_none() {
                        self.confirm_close_pinned = Some(id);
                        self.request_redraw();
                        return;
                    }
                    let remaining = if let Some(host) = self.host.as_ref() {
                        let _ = host.close_tab(id);
                        host.tab_count()
                    } else {
                        0
                    };
                    self.refresh_tab_strip();
                    if remaining == 0 {
                        event_loop.exit();
                    }
                    return;
                }

                let mut enter_visual = false;
                let mut exit_visual = false;
                if let Some(host) = self.host.as_ref()
                    && host.mode() == buffr_core::HostMode::Osr
                    && let Some(cef_button) = winit_button_to_cef(&button)
                {
                    let mouse_up = state == winit::event::ElementState::Released;
                    // Track held mouse buttons so subsequent CursorMoved
                    // events carry the *_MOUSE_BUTTON event flag — without
                    // it, Chromium's hit-test treats drag-motion as plain
                    // hover and won't extend the text selection.
                    let btn_flag: u32 = if cef_button == MouseButtonType::LEFT {
                        16
                    } else if cef_button == MouseButtonType::MIDDLE {
                        32
                    } else if cef_button == MouseButtonType::RIGHT {
                        64
                    } else {
                        0
                    };
                    if mouse_up {
                        self.osr_mouse_buttons &= !btn_flag;
                    } else {
                        self.osr_mouse_buttons |= btn_flag;
                    }
                    // Double-click detection.
                    let now = Instant::now();
                    let same_button = self
                        .osr_last_click_button
                        .map(|b| b == cef_button)
                        .unwrap_or(false);
                    if !mouse_up {
                        if same_button
                            && now.duration_since(self.osr_last_click_at) < DOUBLE_CLICK_WINDOW
                        {
                            self.osr_click_count = (self.osr_click_count + 1).min(3);
                        } else {
                            self.osr_click_count = 1;
                        }
                        self.osr_last_click_at = now;
                        self.osr_last_click_button = Some(cef_button);
                        // Promote CEF widget focus on the first real
                        // click into the OSR region. We deliberately
                        // skip set_focus(1) on load so page-autofocus
                        // doesn't drive a caret-blink paint loop; this
                        // is the place the user finally tells CEF the
                        // page is theirs to interact with.
                        host.osr_focus(true);
                        // Left-click is a user gesture that may focus
                        // an input — allow the next focusin to enter
                        // Insert mode.
                        if button == MouseButton::Left {
                            self.insert_intent_at = Some(Instant::now());
                            // Track drag origin so a left-button release
                            // far from the press point promotes the
                            // engine to Visual mode (CEF natively
                            // selects the swept text).
                            self.osr_drag_start = Some(self.osr_cursor);
                        }
                    } else if button == MouseButton::Left {
                        // osr_drag_start = Some at release ⇒ press did
                        // not cross the drag threshold (CursorMoved would
                        // have cleared it). That's a click — branch on
                        // click_count. None means a drag already promoted
                        // to Visual during the move; nothing to do.
                        if self.osr_drag_start.take().is_some() {
                            if self.osr_click_count >= 2 {
                                // Double / triple click — CEF auto-selects
                                // a word / line. Reflect that in the
                                // engine.
                                enter_visual = true;
                                tracing::debug!(
                                    n = self.osr_click_count,
                                    "osr multi-click → Visual mode"
                                );
                            } else {
                                // Single click. Drop Visual if active.
                                // Clicking an input still goes to Insert
                                // via the JS focusin path.
                                exit_visual = true;
                            }
                        }
                    }
                    let mods = winit_mods_to_cef(&self.modifiers) | self.osr_mouse_buttons;
                    let (bx, by) = self.osr_cursor;
                    host.osr_mouse_click(bx, by, cef_button, mouse_up, self.osr_click_count, mods);
                }
                if enter_visual {
                    if let Ok(mut e) = self.engine.lock() {
                        e.set_mode(PageMode::Visual);
                    }
                    self.refresh_title();
                    self.request_redraw();
                } else if exit_visual {
                    let was_visual = self
                        .engine
                        .lock()
                        .map(|e| e.mode() == PageMode::Visual)
                        .unwrap_or(false);
                    if was_visual {
                        if let Ok(mut e) = self.engine.lock() {
                            e.set_mode(PageMode::Normal);
                        }
                        self.refresh_title();
                        self.request_redraw();
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                use winit::event::MouseScrollDelta;
                if !self
                    .host
                    .as_ref()
                    .map(|h| h.mode() == buffr_core::HostMode::Osr)
                    .unwrap_or(false)
                {
                    return;
                }

                // Two-finger horizontal-swipe back/forward — only on
                // touchpad PixelDelta. If a swipe commits or we're still
                // mid-gesture after a commit, swallow the event so it
                // doesn't also scroll the page.
                if let MouseScrollDelta::PixelDelta(px) = delta {
                    if let Some(action) = self.detect_swipe(px.x as f32, px.y as f32) {
                        self.dispatch_action(&action);
                        return;
                    }
                    if self.swipe_committed {
                        return;
                    }
                }

                let host = self.host.as_ref().unwrap();
                let (dx, dy, is_pixel) = winit_wheel_to_cef_delta(&delta);
                if is_pixel {
                    // Track velocity only for high-res input; discrete
                    // wheel ticks have their own physical inertia.
                    self.osr_wheel_velocity = (dx as f32, dy as f32);
                    self.osr_wheel_last_at = Some(Instant::now());
                } else {
                    // Cancel any in-flight momentum on discrete tick.
                    self.osr_wheel_velocity = (0.0, 0.0);
                    self.osr_wheel_last_at = None;
                }
                let mods = winit_mods_to_cef(&self.modifiers);
                let (bx, by) = self.osr_cursor;
                tracing::trace!(dx, dy, bx, by, "input: mouse_wheel -> CEF");
                host.osr_mouse_wheel(bx, by, dx, dy, mods);
            }
            WindowEvent::KeyboardInput { event, .. } => {
                // Pinned-close confirmation takes precedence over
                // everything else: `y` or `<Enter>` confirms, `n` /
                // `<Esc>` dismisses. Other keys are swallowed so the
                // page underneath can't receive stray input while a
                // modal banner is up.
                if self.confirm_close_pinned.is_some() && self.confirm_handle_key(&event) {
                    return;
                }
                // Permissions prompt takes precedence over every other
                // key sink. Pressing `a`/`d`/`A`/`D`/Esc resolves the
                // request; nothing else is allowed through until the
                // queue drains.
                if self.permissions_handle_key(&event) {
                    return;
                }
                // Overlay open → all keys route to it.
                if self.overlay_handle_key(&event) {
                    return;
                }
                // Hint mode: route printable chars + Esc + BS straight
                // to the host's hint-session API. The modal engine
                // already sits in `Mode::Hint` (set by the action
                // dispatch below), but the engine itself doesn't know
                // about per-keystroke hint matching.
                if self.hint_mode_handle_key(&event) {
                    return;
                }
                // Edit-mode takes precedence over the page-mode FSM
                // once a field is focused (Editing state). Esc is
                // intercepted; all other keys forward directly to CEF.
                if matches!(&self.edit_focus, EditFocus::Editing { .. })
                    && self.edit_mode_handle_key(&event)
                {
                    return;
                }
                // Page-mode dispatch accepts auto-repeat events so
                // holding e.g. `H` / `L` cycles tabs at OS repeat speed.
                // Per-action filtering happens after resolution: see
                // `PageAction::is_repeatable`.
                let is_repeat = event.repeat;
                let Some(chord) = key_event_to_chord_with_repeat(&event, self.modifiers) else {
                    return;
                };
                let now = self.startup.elapsed();
                let (step, post_mode) = match self.engine.lock() {
                    Ok(mut e) => {
                        let s = e.feed(chord, now);
                        let m = e.mode();
                        (s, m)
                    }
                    Err(_) => return,
                };
                match step {
                    Step::Resolved(action) => {
                        // Drop auto-repeat events for actions that
                        // shouldn't stream (TabClose, OpenOmnibar, etc).
                        if is_repeat && !action.is_repeatable() {
                            return;
                        }
                        // `EnterInsertMode` (`i`) flips the engine into
                        // PageMode::Insert. Entry into a specific field is
                        // handled via the JS focusin bridge; `i` alone
                        // without a focused input is a no-op at the page
                        // level — the engine mode flip is sufficient to
                        // unblock subsequent keys once a field is clicked.
                        if action == buffr_modal::PageAction::EnterInsertMode {
                            self.refresh_title();
                            return;
                        }
                        // OpenOmnibar / OpenCommandLine flip the
                        // engine into Mode::Command and ALSO open the
                        // matching overlay UI. The host's `dispatch`
                        // for these is a no-op log, so we handle the
                        // UI side here.
                        match &action {
                            buffr_modal::PageAction::OpenOmnibar => {
                                self.open_omnibar();
                            }
                            buffr_modal::PageAction::OpenCommandLine => {
                                self.open_command_line();
                            }
                            buffr_modal::PageAction::Find { forward } => {
                                self.open_find(*forward);
                            }
                            _ => {
                                self.dispatch_action(&action);
                            }
                        }
                    }
                    Step::Pending | Step::Ambiguous { .. } => {
                        // Phase 3 chrome will surface a count/pending
                        // buffer indicator in the status line. For
                        // now, silently accumulate.
                    }
                    Step::Reject => {
                        // Vim-style: only pass unbound keys to the page
                        // in modes where the page owns input (Edit /
                        // Command). In Normal, Visual, Hint, and Pending
                        // the modal layer owns the keyboard — silently
                        // swallow so typing `a`, `s`, etc. doesn't
                        // type into a focused field or trigger browser
                        // shortcuts.
                        let pass_through =
                            matches!(post_mode, PageMode::Insert | PageMode::Command);
                        if pass_through {
                            if let Some(host) = self.host.as_ref() {
                                let mods = winit_mods_to_cef(&self.modifiers);
                                for ev in winit_key_to_cef_events(&event, mods) {
                                    host.osr_key_event(ev);
                                }
                            }
                        } else {
                            trace!(
                                ?chord,
                                ?post_mode,
                                "key not bound — swallowed in modal mode"
                            );
                        }
                    }
                    Step::EditModeActive => {
                        // Engine is already in PageMode::Insert; consume
                        // the key. If a field is focused, edit_mode_handle_key
                        // above already handled it; otherwise the key is
                        // silently dropped (no input is active).
                    }
                }
                self.refresh_title();
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Ctrl+C single-press exit: the ctrlc handler sets this flag;
        // we check it here before doing any other work so the exit is
        // clean (session saved, CEF not left in a wedged state).
        if self.shutdown_flag.load(Ordering::SeqCst) {
            self.save_session_now();
            event_loop.exit();
            return;
        }

        // Pump CEF every frame. On macOS native windowed CEF integrates
        // with AppKit; calling CefDoMessageLoopWork from inside winit's
        // AppKit event handler can re-enter winit and trip its macOS
        // reentrancy guard.
        pump_cef_message_loop(&mut self.cef_next_pump_at);

        // Wheel-momentum tick: synthesize a decaying wheel event once
        // high-res input has gone quiet, mimicking native Chrome's
        // post-swipe ease-out. No-op while real input is still arriving.
        self.tick_wheel_momentum();

        // Edit-mode: drain focus/blur/mutate events from the JS bridge.
        // Runs before the engine tick so state is consistent when key
        // routing fires later in the same event-loop iteration.
        self.drain_edit_focus_events();
        // Defer-then-flip for Tab transfer: if the grace window after a
        // Blur expired without a sibling Focus arriving, finalize the
        // exit from Insert mode now.
        self.expire_pending_blur();

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

        // Address-change events: drain URL updates pushed by
        // on_address_change. No CEF call; purely reads from the shared
        // VecDeque. Fires before find so Tab.url is current.
        if let Some(host) = self.host.as_ref()
            && host.pump_address_changes()
        {
            self.mark_session_dirty();
            // edit.js is re-injected on each new page load, which
            // reassigns all field IDs from f1. Any saved ID is stale.
            self.last_focused_field = None;
            self.request_redraw();
        }

        // Drain any find result the CEF browser thread posted since
        // the last tick, then check whether the `--find` smoke
        // dispatch is due.
        self.pump_find_results();
        self.maybe_dispatch_find_smoke();
        self.maybe_dispatch_find_live();

        // Drain any hint event (Ready / Error from the renderer) and
        // refresh the statusline indicator off the live session.
        if let Some(host) = self.host.as_ref() {
            if host.pump_hint_events() {
                self.request_redraw();
            }
            let new_status = host.hint_status().map(|h| UiHintStatus {
                typed: h.typed,
                match_count: h.match_count as u32,
                background: h.background,
            });
            if new_status != self.statusline.hint_state {
                self.statusline.hint_state = new_status;
                self.mark_chrome_dirty();
                self.request_redraw();
            }
        }

        // CEF popup re-route: drain URLs queued by on_before_popup for
        // NEW_FOREGROUND_TAB / NEW_BACKGROUND_TAB dispositions and open
        // each as a tab. Popup-window dispositions (OAuth, etc) are not
        // queued — CEF handles those natively.
        if let Some(host) = self.host.as_ref() {
            for url in drain_popup_urls(&host.popup_queue()) {
                if let Err(err) = host.open_tab(&url) {
                    warn!(error = %err, %url, "popup -> open_tab failed");
                }
            }
        }

        // Popup create: drain PopupCreated events and spawn a winit window
        // + wgpu renderer for each new popup browser.
        let popup_creates = drain_popup_creates(&self.popup_create_sink);
        for created in popup_creates {
            let title = if created.url.is_empty() {
                "buffr popup".to_string()
            } else {
                created.url.clone()
            };
            let win_attrs = Window::default_attributes()
                .with_title(&title)
                .with_inner_size(winit::dpi::LogicalSize::new(800u32, 600u32))
                .with_decorations(true);
            let popup_win = match event_loop.create_window(win_attrs) {
                Ok(w) => Arc::new(w),
                Err(err) => {
                    warn!(error = %err, browser_id = created.browser_id, "popup: create_window failed");
                    continue;
                }
            };
            let popup_renderer = match crate::render::Renderer::new(popup_win.clone()) {
                Ok(r) => r,
                Err(err) => {
                    warn!(error = %err, browser_id = created.browser_id, "popup: renderer init failed");
                    continue;
                }
            };
            // Initial OSR resize to match the window's actual inner size.
            let inner = popup_win.inner_size();
            let pw = inner.width.max(1);
            let ph = inner.height.max(1);
            if let Some(host) = self.host.as_ref() {
                host.popup_resize(created.browser_id, pw, ph);
            }
            // Wire OSR on_paint → popup window redraw via EventLoopProxy.
            let proxy = self.event_proxy.clone();
            let bid = created.browser_id;
            created.view.set_wake(Arc::new(move || {
                let _ = proxy.send_event(BuffrUserEvent::OsrFramePopup(bid));
            }));
            let wid = popup_win.id();
            debug!(
                browser_id = created.browser_id,
                ?wid,
                "popup: window created"
            );
            self.popup_window_id_by_browser
                .insert(created.browser_id, wid);
            self.popups.insert(
                wid,
                PopupWindow {
                    window: popup_win,
                    renderer: popup_renderer,
                    browser_id: created.browser_id,
                    frame: created.frame,
                    view: created.view,
                    url: created.url,
                    last_osr_generation: 0,
                    osr_scratch: Vec::new(),
                    chrome_generation: 1,
                    last_painted_chrome_gen: 0,
                    cursor: (0, 0),
                    mouse_buttons: 0,
                    modifiers: ModifiersState::empty(),
                    last_click_at: Instant::now(),
                    last_click_button: None,
                    click_count: 1,
                },
            );
        }

        // Popup close: drain browser-id events and drop their windows.
        let popup_closes: Vec<i32> = drain_popup_closes(&self.popup_close_sink);
        for browser_id in popup_closes {
            if let Some(wid) = self.popup_window_id_by_browser.remove(&browser_id) {
                self.popups.remove(&wid);
                debug!(browser_id, "popup: window dropped");
            }
        }

        // Popup URL updates: drain address-change events for popup browsers
        // and update the corresponding popup window's URL bar.
        let popup_addr_changes: Vec<(i32, String)> = if let Some(host) = self.host.as_ref() {
            host.popup_drain_address_changes()
        } else {
            Vec::new()
        };
        for (browser_id, url) in popup_addr_changes {
            if let Some(&wid) = self.popup_window_id_by_browser.get(&browser_id)
                && let Some(popup) = self.popups.get_mut(&wid)
                && popup.url != url
            {
                popup.url = url.clone();
                popup.chrome_generation = popup.chrome_generation.wrapping_add(1);
                popup.window.request_redraw();
                debug!(browser_id, %url, "popup: URL updated");
            }
        }

        // Popup title updates: drain title-change events for popup browsers
        // and update the winit window title.
        let popup_title_changes: Vec<(i32, String)> = if let Some(host) = self.host.as_ref() {
            host.popup_drain_title_changes()
        } else {
            Vec::new()
        };
        for (browser_id, title) in popup_title_changes {
            if let Some(&wid) = self.popup_window_id_by_browser.get(&browser_id)
                && let Some(popup) = self.popups.get(&wid)
            {
                popup.window.set_title(&title);
                debug!(browser_id, %title, "popup: title updated");
            }
        }

        // Permission prompt: pull the front of the queue into a
        // visible widget. `sync_permissions_prompt` is a no-op when a
        // prompt is already active, so the user always sees one
        // request at a time.
        if self.sync_permissions_prompt() {
            self.mark_chrome_dirty();
            self.request_redraw();
        }

        // Live URL / zoom sync: throttled to ~4 Hz (250 ms).
        // URL is now cheap (reads cached Tab.url; no CEF call).
        // Zoom polls host.zoom_level() at the same cadence.
        // Also detects navigation, active-index, and tab-list changes
        // for the session dirty flag.
        // Collect the poll results outside the borrow so we can call
        // `mark_session_dirty` (which takes &mut self) afterwards.
        let url_poll_result: Option<(String, Option<usize>, Vec<buffr_core::TabId>, f64)> =
            if let Some(host) = self.host.as_ref() {
                let now = Instant::now();
                if now.duration_since(self.last_url_poll) >= Duration::from_millis(250) {
                    self.last_url_poll = now;
                    let live = host.active_tab_live_url();
                    let active_idx = host.active_index();
                    let current_ids: Vec<buffr_core::TabId> =
                        host.tabs_summary().iter().map(|t| t.id).collect();
                    let zoom = host.active_zoom_level();
                    Some((live, active_idx, current_ids, zoom))
                } else {
                    None
                }
            } else {
                None
            };
        if let Some((live, active_idx, current_ids, zoom)) = url_poll_result {
            if !live.is_empty() && live != self.statusline.url {
                self.statusline.url = live.clone();
                self.refresh_title();
                self.mark_chrome_dirty();
                self.request_redraw();
            }
            // Session dirty detection: URL changed since last save.
            if !live.is_empty() && live != self.last_session_url {
                self.mark_session_dirty();
            }
            // Active-index changed.
            if active_idx != self.last_session_active {
                tracing::debug!(
                    new_idx = ?active_idx,
                    last_idx = ?self.last_session_active,
                    "session: active-index changed -> mark_session_dirty"
                );
                self.mark_session_dirty();
            }
            // Tab-list (open / close / reorder) changed.
            if current_ids != self.last_session_tab_ids {
                self.mark_session_dirty();
            }
            // Zoom level: poll active tab and update statusline.
            if (zoom - self.statusline.zoom_level).abs() > f64::EPSILON {
                self.statusline.zoom_level = zoom;
                self.mark_chrome_dirty();
                self.request_redraw();
            }
        }

        // Flush session when dirty and the debounce window has expired.
        // Shutdown paths (CloseRequested, last-tab-gone, ctrl-c) call
        // `save_session_now` directly, bypassing this check.
        if self.session_dirty {
            let debounce = Duration::from_millis(SESSION_SAVE_DEBOUNCE_MS);
            let elapsed_enough = self
                .session_dirty_since
                .map(|t| t.elapsed() >= debounce)
                .unwrap_or(true);
            if elapsed_enough {
                self.save_session_now();
            }
        }

        // Download notices: drop any that have lived past their expiry
        // window. Trigger a redraw + resync when the queue changes so
        // the chrome immediately reclaims the strip height.
        {
            let dropped = expire_stale_notices(&self.download_notice_queue);
            if dropped > 0 {
                self.resync_cef_rect();
                self.mark_chrome_dirty();
                self.request_redraw();
            }
        }

        // Refresh tab-strip render input. The host's tab list can
        // change underneath us (LoadHandler updates URL/title;
        // dispatched tab actions add/remove rows) so we resync every
        // tick. The cost is a small alloc; the redraw is gated on
        // diff via softbuffer's damage rect.
        let prev_tabs = self.tab_strip.tabs.clone();
        let prev_active = self.tab_strip.active;
        self.refresh_tab_strip();
        if prev_tabs != self.tab_strip.tabs || prev_active != self.tab_strip.active {
            self.request_redraw();
        }

        // Phase 6 telemetry: 60-second background flush so an abrupt
        // exit (segfault from CEF, OOM kill, etc.) loses at most one
        // minute of counter increments. No-op when disabled.
        let wall_now = Instant::now();
        if wall_now.duration_since(self.counters_flush_at) >= Duration::from_secs(60) {
            self.counters_flush_at = wall_now;
            self.counters.flush();
        }

        // Cursor blink for the open overlay. 500ms toggle; we only
        // request a redraw when the bit actually flips so the page
        // region isn't repainted needlessly.
        if self.overlay.is_some() {
            let now = Instant::now();
            if now.duration_since(self.cursor_blink_at) >= Duration::from_millis(500) {
                self.cursor_blink_at = now;
                if let Some(overlay) = self.overlay.as_mut() {
                    let bar = overlay.input_mut();
                    bar.cursor_visible = !bar.cursor_visible;
                }
                self.mark_chrome_dirty();
                self.request_redraw();
            }
        }

        // No idle paint loop: we respect Wayland's frame-callback model
        // and only repaint on explicit `request_redraw` (e.g. resize,
        // input, mode/url change). OSR `on_paint` updates that arrive
        // while no redraw is queued show on the next compositor-driven
        // frame.

        // Cap the event-loop wakeup cadence at the display's refresh
        // rate so CEF's message pump (which needs regular service)
        // stops pinning the main thread at 100% CPU. Real-time
        // wakeups (input, OSR on_paint → EventLoopProxy, Resized)
        // preempt the deadline; the deadline only fires when nothing
        // else woke us. wgpu's surface itself runs Fifo (vsync) so
        // render rate is already capped to display refresh; this
        // matches the pump cadence to it.
        let frame_period = self
            .window
            .as_ref()
            .and_then(|w| w.current_monitor())
            .and_then(|m| m.refresh_rate_millihertz())
            .filter(|&mhz| mhz > 0)
            .map(|mhz| Duration::from_nanos(1_000_000_000_000 / u64::from(mhz)))
            .unwrap_or(Duration::from_millis(16));
        let next_wakeup = Instant::now() + frame_period;
        // If CEF has scheduled a pump, wake up no later than that.
        let deadline = match self.cef_next_pump_at {
            Some(at) if at < next_wakeup => at,
            _ => next_wakeup,
        };
        event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
    }
}

#[cfg(target_os = "macos")]
fn pump_cef_message_loop(next_pump_at: &mut Option<Instant>) {
    if let Some(delay_ms) = buffr_core::take_scheduled_message_pump_delay_ms() {
        let delay = Duration::from_millis(delay_ms.try_into().unwrap_or(0));
        let at = Instant::now() + delay;
        tracing::trace!(delay_ms, ?at, "cef: schedule next pump");
        *next_pump_at = Some(at);
    }
    if let Some(at) = *next_pump_at {
        if Instant::now() >= at {
            tracing::trace!("cef: do_message_loop_work");
            cef::do_message_loop_work();
            *next_pump_at = None;
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn pump_cef_message_loop(_next_pump_at: &mut Option<Instant>) {
    cef::do_message_loop_work();
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

    #[test]
    fn resolve_paths_private_creates_subdirs_and_returns_tempdir() {
        let (paths, tmp) = resolve_paths(true).expect("resolve_paths(true)");
        let tmp = tmp.expect("private mode returns Some(TempDir)");
        assert!(paths.cache.starts_with(tmp.path()));
        assert!(paths.data.starts_with(tmp.path()));
        assert!(paths.cache.exists());
        assert!(paths.data.exists());
        assert!(paths.cache.ends_with("cache"));
        assert!(paths.data.ends_with("data"));
        // Drop tempdir → tree gone.
        let dir_path = tmp.path().to_path_buf();
        drop(tmp);
        assert!(!dir_path.exists());
    }

    #[test]
    fn resolve_paths_persistent_returns_no_tempdir() {
        let (_paths, tmp) = resolve_paths(false).expect("resolve_paths(false)");
        assert!(tmp.is_none());
    }

    // ---- edit-mode unit tests --------------------------------------------

    /// Build a minimal `winit::keyboard::ModifiersState` for tests.
    fn no_mods() -> winit::keyboard::ModifiersState {
        winit::keyboard::ModifiersState::empty()
    }

    /// Tests for `winit_key_to_planned`. We can't construct `winit::event::KeyEvent`
    /// directly in unit tests (the `platform_specific` field is opaque on Linux),
    /// so we extract the inner translation logic into a free function that takes
    /// `(logical_key, modifiers, pressed)` — the three inputs `winit_key_to_planned`
    /// gates on — and test through that seam.
    mod winit_key_to_planned_tests {
        use super::*;
        use winit::keyboard::{Key as WKey, NamedKey as WNamed, SmolStr};

        /// Thin mirror of `AppState::winit_key_to_planned` that accepts the
        /// logical key directly, sidestepping the `KeyEvent` construction.
        fn translate(
            logical: &WKey<SmolStr>,
            modifiers: winit::keyboard::ModifiersState,
            pressed: bool,
        ) -> Option<PlannedInput> {
            if !pressed {
                return None;
            }
            let mods = EngineModifiers {
                ctrl: modifiers.control_key(),
                shift: modifiers.shift_key(),
                alt: modifiers.alt_key(),
                super_: modifiers.super_key(),
            };
            match logical {
                WKey::Character(s) => {
                    let mut chars = s.chars();
                    let first = chars.next()?;
                    if chars.next().is_some() {
                        return None;
                    }
                    Some(PlannedInput::Char(first, mods))
                }
                WKey::Named(named) => {
                    let sk = match named {
                        WNamed::Escape => SpecialKey::Esc,
                        WNamed::Enter => SpecialKey::Enter,
                        WNamed::Backspace => SpecialKey::Backspace,
                        WNamed::Tab => SpecialKey::Tab,
                        WNamed::ArrowUp => SpecialKey::Up,
                        WNamed::ArrowDown => SpecialKey::Down,
                        WNamed::ArrowLeft => SpecialKey::Left,
                        WNamed::ArrowRight => SpecialKey::Right,
                        WNamed::Home => SpecialKey::Home,
                        WNamed::End => SpecialKey::End,
                        WNamed::PageUp => SpecialKey::PageUp,
                        WNamed::PageDown => SpecialKey::PageDown,
                        WNamed::Insert => SpecialKey::Insert,
                        WNamed::Delete => SpecialKey::Delete,
                        WNamed::F1 => SpecialKey::F(1),
                        WNamed::F2 => SpecialKey::F(2),
                        WNamed::F3 => SpecialKey::F(3),
                        WNamed::F4 => SpecialKey::F(4),
                        WNamed::F5 => SpecialKey::F(5),
                        WNamed::F6 => SpecialKey::F(6),
                        WNamed::F7 => SpecialKey::F(7),
                        WNamed::F8 => SpecialKey::F(8),
                        WNamed::F9 => SpecialKey::F(9),
                        WNamed::F10 => SpecialKey::F(10),
                        WNamed::F11 => SpecialKey::F(11),
                        WNamed::F12 => SpecialKey::F(12),
                        _ => return None,
                    };
                    Some(PlannedInput::Key(sk, mods))
                }
                _ => None,
            }
        }

        #[test]
        fn char_a_maps_to_planned_char() {
            let p = translate(&WKey::Character(SmolStr::new("a")), no_mods(), true);
            assert!(matches!(p, Some(PlannedInput::Char('a', _))));
        }

        #[test]
        fn esc_maps_to_planned_esc() {
            let p = translate(&WKey::Named(WNamed::Escape), no_mods(), true);
            assert!(matches!(p, Some(PlannedInput::Key(SpecialKey::Esc, _))));
        }

        #[test]
        fn enter_maps_to_planned_enter() {
            let p = translate(&WKey::Named(WNamed::Enter), no_mods(), true);
            assert!(matches!(p, Some(PlannedInput::Key(SpecialKey::Enter, _))));
        }

        #[test]
        fn tab_maps_to_planned_tab() {
            let p = translate(&WKey::Named(WNamed::Tab), no_mods(), true);
            assert!(matches!(p, Some(PlannedInput::Key(SpecialKey::Tab, _))));
        }

        #[test]
        fn arrows_map_correctly() {
            let cases = [
                (WNamed::ArrowUp, SpecialKey::Up),
                (WNamed::ArrowDown, SpecialKey::Down),
                (WNamed::ArrowLeft, SpecialKey::Left),
                (WNamed::ArrowRight, SpecialKey::Right),
            ];
            for (named, expected) in cases {
                let p = translate(&WKey::Named(named), no_mods(), true);
                assert!(
                    matches!(p, Some(PlannedInput::Key(sk, _)) if sk == expected),
                    "expected {expected:?} for {named:?}"
                );
            }
        }

        #[test]
        fn f1_maps_to_planned_f1() {
            let p = translate(&WKey::Named(WNamed::F1), no_mods(), true);
            assert!(matches!(p, Some(PlannedInput::Key(SpecialKey::F(1), _))));
        }

        #[test]
        fn release_returns_none() {
            let p = translate(&WKey::Character(SmolStr::new("a")), no_mods(), false);
            assert!(p.is_none());
        }
    }

    /// Test the `EditFocus` FSM state transitions (None ↔ Editing).
    mod edit_focus_fsm_tests {
        use super::*;
        use buffr_core::edit::{EditFieldKind, new_edit_event_sink};

        fn push_event(sink: &EditEventSink, ev: EditConsoleEvent) {
            sink.lock().unwrap().push_back(ev);
        }

        fn focus_event(id: &str) -> EditConsoleEvent {
            EditConsoleEvent::Focus {
                field_id: id.to_string(),
                kind: EditFieldKind::Input,
                value: "hello".to_string(),
                selection_start: Some(5),
                selection_end: Some(5),
            }
        }

        fn blur_event(id: &str) -> EditConsoleEvent {
            EditConsoleEvent::Blur {
                field_id: id.to_string(),
            }
        }

        fn mutate_event(id: &str, val: &str) -> EditConsoleEvent {
            EditConsoleEvent::Mutate {
                field_id: id.to_string(),
                value: val.to_string(),
            }
        }

        /// Minimal inline drain that mirrors `drain_edit_focus_events`.
        fn drain_into(focus: &mut EditFocus, evs: Vec<EditConsoleEvent>) {
            for ev in evs {
                match ev {
                    EditConsoleEvent::Focus { field_id, .. } => {
                        let already = matches!(
                            &*focus,
                            EditFocus::Editing { field_id: f } if *f == field_id
                        );
                        if !already {
                            *focus = EditFocus::Editing { field_id };
                        }
                    }
                    EditConsoleEvent::Blur { field_id } => {
                        if matches!(&*focus, EditFocus::Editing { field_id: f } if *f == field_id) {
                            *focus = EditFocus::None;
                        }
                    }
                    EditConsoleEvent::Mutate { .. } => {}
                    EditConsoleEvent::Selection { .. } => {}
                }
            }
        }

        #[test]
        fn focus_moves_to_editing() {
            let sink = new_edit_event_sink();
            push_event(&sink, focus_event("f1"));
            let evs = drain_edit_events(&sink);

            let mut focus = EditFocus::None;
            drain_into(&mut focus, evs);
            assert!(matches!(&focus, EditFocus::Editing { field_id } if field_id == "f1"));
        }

        #[test]
        fn blur_resets_to_none() {
            let sink = new_edit_event_sink();
            push_event(&sink, focus_event("f1"));
            push_event(&sink, blur_event("f1"));
            let evs = drain_edit_events(&sink);

            let mut focus = EditFocus::None;
            drain_into(&mut focus, evs);
            assert!(matches!(focus, EditFocus::None));
        }

        #[test]
        fn mutate_while_editing_is_ignored() {
            // Mutate events are no-ops in the simplified FSM — just verify
            // focus state is unchanged after receiving one.
            let sink = new_edit_event_sink();
            push_event(&sink, focus_event("f1"));
            push_event(&sink, mutate_event("f1", "world"));
            let evs = drain_edit_events(&sink);

            let mut focus = EditFocus::None;
            drain_into(&mut focus, evs);
            // Still Editing; the mutate was silently consumed.
            assert!(matches!(&focus, EditFocus::Editing { field_id } if field_id == "f1"));
        }

        #[test]
        fn blur_on_wrong_field_does_not_reset() {
            let sink = new_edit_event_sink();
            push_event(&sink, focus_event("f1"));
            push_event(&sink, blur_event("f99")); // different field
            let evs = drain_edit_events(&sink);

            let mut focus = EditFocus::None;
            drain_into(&mut focus, evs);
            // f1 still active; blur for f99 was a no-op.
            assert!(matches!(&focus, EditFocus::Editing { field_id } if field_id == "f1"));
        }
    }
}
