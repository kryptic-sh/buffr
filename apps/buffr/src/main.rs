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

use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use buffr_config::{ClearableData, Config, ConfigSource};
use buffr_core::cmdline::{Command, parse as parse_cmdline};
use buffr_core::{
    BuffrApp, DownloadNoticeQueue, EditConsoleEvent, EditEventSink, FindResultSink, HintAction,
    HintAlphabet, HintEventSink, NEW_TAB_URL, PermissionsQueue, PromptOutcome, TabId,
    drain_edit_events, drain_permissions_with_defer, expire_stale_notices, init_cef_api,
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
    HintStatus as UiHintStatus, InputBar, PERMISSIONS_PROMPT_HEIGHT, PermissionsPrompt,
    STATUSLINE_HEIGHT, Statusline, Suggestion, SuggestionKind, TAB_STRIP_HEIGHT, TabStrip, TabView,
};

mod session;
use cef::{ImplBrowser, KeyEvent, KeyEventType, MouseButtonType, Settings};
use clap::Parser;
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use tempfile::TempDir;
use tracing::{info, trace, warn};
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

    // -------- profile paths (persistent) or tempdir (--private) ------
    //
    // Private mode replaces both `cache` and `data` with a freshly-
    // created `TempDir` under `$TMPDIR/buffr-private-<pid>`. The dir
    // is deleted by `Drop` at process exit. Stores are opened
    // in-memory, so no SQLite file ever appears on disk.
    let (paths, _private_tmp) = resolve_paths(cli.private)?;
    if cli.private {
        info!(
            cache = %paths.cache.display(),
            data = %paths.data.display(),
            "private mode active — no data persists across restart"
        );
    } else {
        info!(cache = %paths.cache.display(), data = %paths.data.display(), "profile paths");
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
        info!(path = %dir.display(), "downloads default_dir resolved");
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
    let settings = Settings {
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

    // Register the `buffr://` scheme handler factory so that internal URLs
    // such as `buffr://new` (the new-tab page) are served by Rust code
    // rather than delegated to the network stack.
    register_buffr_handler_factory();

    // Phase 6 telemetry: count the successful CEF init as one
    // `app_starts` event. No-op when disabled. We tick *after*
    // `cef::initialize` returns 1 so a launch that crashes during CEF
    // boot doesn't get counted as a successful start.
    counters.increment(buffr_core::KEY_APP_STARTS);

    // -------- winit event loop --------
    //
    // Allow winit to pick the best Wayland backend. Linux always uses
    // HostMode::Osr (softbuffer composite over Wayland) — X11/XWayland
    // windowed embedding is not supported. macOS and Windows use native
    // child-window embedding (HostMode::Windowed).
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
    let pending_session_tabs: Vec<session::PersistedTab> = if cli.private || cli.no_restore {
        Vec::new()
    } else if let Some(p) = session_path.as_ref() {
        match session::read(p) {
            Ok(Some(s)) => {
                info!(
                    path = %p.display(),
                    tabs = s.tabs.len(),
                    "session: restored",
                );
                s.tabs
            }
            Ok(None) => Vec::new(),
            Err(err) => {
                warn!(error = %err, "session: read failed — starting fresh");
                Vec::new()
            }
        }
    } else {
        Vec::new()
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
        session_path,
        counters.clone(),
        update_checker.clone(),
        initial_update_status,
        config.theme.high_contrast,
        shutdown_flag,
    );
    if let Err(err) = event_loop.run_app(&mut app_state) {
        warn!(error = %err, "winit event loop exited with error");
    }

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
    info!("cef shutting down");
    cef::shutdown();
    // Tempdir drops here (after CEF is gone), removing the private
    // profile root tree.
    drop(_private_tmp);
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
            for tab in &s.tabs {
                let pin = if tab.pinned { "*" } else { " " };
                println!("{pin}\t{}", tab.url);
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
        Ok((paths, None))
    }
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
    homepage: String,
    window: Option<Arc<Window>>,
    host: Option<buffr_core::BrowserHost>,
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
    pending_session_tabs: Vec<session::PersistedTab>,
    /// Path the runtime persists the live tab list to on clean
    /// shutdown. `None` in private mode (sessions never persist).
    session_path: Option<PathBuf>,
    /// `softbuffer` graphics context. `Surface` is per-window; the
    /// context can be reused across windows if we ever spawn more.
    softbuffer_ctx: Option<softbuffer::Context<Arc<Window>>>,
    softbuffer_surface: Option<softbuffer::Surface<Arc<Window>, Arc<Window>>>,
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
    /// Timestamp of the last OSR redraw request.  Used by `about_to_wait`
    /// to throttle to ≈60 Hz while paint→redraw signalling is still polled.
    /// TODO: replace with a direct signal from `OsrPaintHandler::on_paint`
    ///       once a cross-thread wakeup channel (e.g. EventLoopProxy) lands.
    last_osr_redraw: Instant,
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
    /// Ctrl+C handler flag. Set to `true` by the `ctrlc` handler;
    /// polled in `about_to_wait` to exit with a single key press.
    shutdown_flag: Arc<AtomicBool>,
    /// Ordered list of `TabId`s mirroring `tab_strip.tabs`. Refreshed
    /// every `about_to_wait` tick alongside the strip; used for
    /// tab-strip click hit-testing.
    tab_ids: Vec<TabId>,
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

/// Active overlay above the CEF child window.
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
        pending_session_tabs: Vec<session::PersistedTab>,
        session_path: Option<PathBuf>,
        counters: Arc<buffr_core::UsageCounters>,
        update_checker: Arc<buffr_core::UpdateChecker>,
        initial_update_status: buffr_core::UpdateStatus,
        high_contrast: bool,
        shutdown_flag: Arc<AtomicBool>,
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
            window: None,
            host: None,
            engine,
            history,
            bookmarks,
            downloads,
            downloads_config,
            zoom,
            permissions,
            permissions_queue,
            permissions_prompt: None,
            download_notice_queue,
            search_config,
            overlay: None,
            private,
            modifiers: ModifiersState::empty(),
            startup: Instant::now(),
            current_mode_label: mode_label(PageMode::Normal),
            find_sink,
            hint_sink,
            edit_sink,
            edit_focus: EditFocus::None,
            hint_alphabet,
            pending_find,
            find_smoke_at: None,
            statusline,
            tab_strip: TabStrip::default(),
            pending_new_tabs,
            pending_session_tabs,
            session_path,
            softbuffer_ctx: None,
            softbuffer_surface: None,
            cursor_blink_at: Instant::now(),
            counters,
            counters_flush_at: Instant::now(),
            update_checker,
            last_osr_generation: 0,
            last_osr_redraw: Instant::now(),
            osr_cursor: (0, 0),
            osr_last_click_at: Instant::now(),
            osr_last_click_button: None,
            osr_click_count: 1,
            shutdown_flag,
            tab_ids: Vec::new(),
        }
    }

    /// Window-title prefix. Persistent runs render `buffr — NORMAL`;
    /// private mode inserts a marker between the brand and the mode
    /// stamp so glancing at the taskbar makes the privacy state
    /// obvious: `buffr — PRIVATE — NORMAL`.
    fn title_for(&self, mode_label: &str) -> String {
        if self.private {
            format!("buffr — PRIVATE — {mode_label}")
        } else {
            format!("buffr — {mode_label}")
        }
    }

    fn dispatch_action(&mut self, action: &buffr_modal::PageAction) {
        let Some(host) = self.host.as_ref() else {
            warn!(?action, "no browser host yet — dropping action");
            return;
        };
        // Tab actions need apps-layer policy decisions (e.g. last-tab
        // close → exit) so they bypass the host dispatcher's fallback
        // path.
        use buffr_modal::PageAction as A;
        match action {
            A::TabNew => {
                let url = NEW_TAB_URL;
                if let Err(err) = host.open_tab(url) {
                    warn!(error = %err, %url, "tab_new: failed");
                }
            }
            A::TabClose => {
                self.close_active_tab_or_exit();
            }
            A::TabNext => host.next_tab(),
            A::TabPrev => host.prev_tab(),
            A::DuplicateTab => {
                if let Err(err) = host.duplicate_active() {
                    warn!(error = %err, "duplicate_tab: failed");
                }
            }
            A::PinTab => host.toggle_pin_active(),
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
            _ => host.dispatch(action),
        }
    }

    /// Close the active tab. If it was the last one, signal the
    /// caller to exit. Returns `true` if more tabs remain.
    fn close_active_tab_or_exit(&self) -> bool {
        let Some(host) = self.host.as_ref() else {
            return false;
        };
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

    /// Persist the live tab list synchronously. Called on graceful
    /// shutdown paths (last-tab-close, `:q`, `Ctrl-C`).
    fn save_session_now(&self) {
        let Some(path) = self.session_path.as_ref() else {
            return;
        };
        let Some(host) = self.host.as_ref() else {
            return;
        };
        let summaries = host.tabs_summary();
        let s = session::Session::from_tabs(summaries.iter().map(|t| (t.url.as_str(), t.pinned)));
        if let Err(err) = session::write(path, &s) {
            warn!(error = %err, "session: write failed");
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
        for (i, t) in session.iter().enumerate() {
            if i == 0 {
                // The initial `BrowserHost::new` already loaded tab 0
                // with `homepage`. Navigate the active tab there
                // instead of opening a new one so we don't end up
                // with a stray homepage tab.
                if let Err(err) = host.navigate(&t.url) {
                    warn!(error = %err, url = %t.url, "session: navigate first tab failed");
                }
                continue;
            }
            match host.open_tab_background(&t.url) {
                Ok(_id) => {
                    if t.pinned {
                        host.toggle_pin_active();
                    }
                }
                Err(err) => warn!(error = %err, url = %t.url, "session: open_tab failed"),
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
        self.tab_strip.tabs = tabs;
        self.tab_strip.active = active;
    }

    fn refresh_title(&mut self) {
        let (mode, count) = match self.engine.lock() {
            Ok(e) => (e.mode(), e.count_buffer()),
            Err(_) => (PageMode::Normal, None),
        };
        let label = mode_label(mode);
        if label != self.current_mode_label {
            self.current_mode_label = label;
            if let Some(window) = self.window.as_ref() {
                window.set_title(&self.title_for(label));
            }
        }
        // Statusline reflects mode + count every refresh — both can
        // change between tick callbacks.
        self.statusline.mode = mode;
        self.statusline.count_buffer = count;
        self.request_redraw();
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
            tracing::info!(%query, "find smoke: start_find");
            self.statusline.find_query = Some(FindStatus {
                query: query.clone(),
                current: 0,
                total: 0,
            });
            host.start_find(&query, true);
        }
    }

    fn paint_chrome(&mut self) {
        let Some(window) = self.window.as_ref() else {
            return;
        };
        let size = window.inner_size();
        let width = size.width.max(1);
        let height = size.height.max(1);
        // Precompute geometry-derived inputs before acquiring the
        // softbuffer surface borrow — geometry helpers and Arc-queue
        // peeks all need `&self`; the borrow is released once `surface`
        // and `buf` drop at end of scope.
        let tab_y = self.tab_strip_y(height);
        let prompt_y = self.permissions_prompt_y();
        let notice_y = self.download_notice_y();
        // Snapshot the front of the download notice queue now so we
        // don't have to touch `self` again while `buf` is live.
        let current_notice = peek_download_notice(&self.download_notice_queue);
        // Precompute browser region for the OSR composite so we don't need
        // a `&self` borrow while the surface buffer is live.
        let (_, browser_y, browser_w, browser_h) = self.cef_child_rect(width, height);
        let Some(surface) = self.softbuffer_surface.as_mut() else {
            return;
        };
        let (Some(nz_w), Some(nz_h)) = (NonZeroU32::new(width), NonZeroU32::new(height)) else {
            return;
        };
        if let Err(err) = surface.resize(nz_w, nz_h) {
            warn!(error = %err, "softbuffer resize failed");
            return;
        }
        let mut buf = match surface.buffer_mut() {
            Ok(b) => b,
            Err(err) => {
                warn!(error = %err, "softbuffer buffer_mut failed");
                return;
            }
        };

        // ── OSR composite (Wayland / off-screen mode only) ────────────────
        // For HostMode::Osr CEF paints BGRA into SharedOsrFrame instead of
        // into a native child window.  We blit those pixels into the top
        // region of the softbuffer surface (everything above the chrome strip)
        // then let the chrome painting below overdraw its own region.
        //
        // HostMode::Windowed: CEF owns an X11 child window that sits over the
        // top region — we must NOT write there or we would clobber it.
        let osr_damage: Option<softbuffer::Rect> = if let Some(host) = self.host.as_ref()
            && host.mode() == buffr_core::HostMode::Osr
        {
            // browser_y / browser_w / browser_h precomputed before the
            // surface borrow above to avoid conflicting &self borrows.
            let osr_frame = host.osr_frame();
            let new_gen = osr_frame
                .lock()
                .map(|f| f.generation)
                .unwrap_or(self.last_osr_generation);

            // Blit regardless of generation change — softbuffer may have
            // discarded our previous write on resize, so we always fill the
            // browser region to avoid showing garbage.  The generation check
            // is kept as a future optimisation hook (skip when surfaced buffer
            // is already fresh) but is not applied for correctness here.
            let _ = new_gen; // used below

            if let Ok(frame) = osr_frame.lock() {
                let osr_w = frame.width as usize;
                let osr_h = frame.height as usize;
                let pixels = &frame.pixels;
                let bw = browser_w as usize;
                let bh = browser_h as usize;
                let by = browser_y as usize;

                for row in 0..bh {
                    let dst_row_base = (by + row) * (width as usize);
                    for col in 0..bw {
                        let dst_idx = dst_row_base + col;
                        if row < osr_h && col < osr_w {
                            // BGRA → 0x00RRGGBB
                            let src = row * osr_w * 4 + col * 4;
                            let b = pixels[src] as u32;
                            let g = pixels[src + 1] as u32;
                            let r = pixels[src + 2] as u32;
                            buf[dst_idx] = (r << 16) | (g << 8) | b;
                        } else {
                            // OSR frame smaller than browser region — fill white.
                            buf[dst_idx] = 0x00FF_FFFF;
                        }
                    }
                }
                self.last_osr_generation = frame.generation;
            }

            // Damage the whole browser region so softbuffer presents it.
            NonZeroU32::new(browser_w)
                .zip(NonZeroU32::new(browser_h))
                .map(|(nz_bw, nz_bh)| softbuffer::Rect {
                    x: 0,
                    y: browser_y,
                    width: nz_bw,
                    height: nz_bh,
                })
        } else {
            None
        };

        // Statusline writes only the bottom strip; the input bar (when
        // active) writes only the top strip. Page region in between is
        // owned by the CEF child window and we never touch it. We use
        // `present_with_damage` to avoid blanking the page area —
        // softbuffer 0.4 has no alpha, so writing zeros to the page
        // region would clobber CEF's surface on X11.
        self.statusline
            .paint(buf.as_mut(), width as usize, height as usize);

        // Tab strip — sits between input bar (when open) and CEF
        // page area. Always painted; the buffer slot lives at
        // `tab_y` (precomputed above so the softbuffer borrow holds).
        self.tab_strip
            .paint(buf.as_mut(), width as usize, height as usize, tab_y);

        // Permissions prompt — sits between download notice and tab strip
        // when active. Drawn after the tab strip so its accent border
        // never overlaps a tab pill.
        if let Some(prompt) = self.permissions_prompt.as_ref() {
            prompt.paint(buf.as_mut(), width as usize, height as usize, prompt_y);
        }

        // Download notice strip — sits above the permissions prompt (or
        // above the tab strip when no prompt is active). Drawn last
        // (highest priority visual layer) so its accent border is
        // always visible. `current_notice` was snapshotted before this
        // borrow scope; use it directly.
        if let Some(ref notice) = current_notice {
            let strip = DownloadNoticeStrip {
                kind: match notice.kind {
                    buffr_core::DownloadNoticeKind::Started => DownloadNoticeKind::Started,
                    buffr_core::DownloadNoticeKind::Completed => DownloadNoticeKind::Completed,
                    buffr_core::DownloadNoticeKind::Failed => DownloadNoticeKind::Failed,
                },
                filename: notice.filename.clone(),
                path: notice.path.clone(),
            };
            strip.paint(buf.as_mut(), width as usize, height as usize, notice_y);
        }

        let mut damage = Vec::with_capacity(6);

        // OSR browser-region damage rect (must be added before chrome rects
        // so that chrome always renders on top in the damage list order).
        if let Some(osr_rect) = osr_damage {
            damage.push(osr_rect);
        }

        // Statusline damage rect (bottom).
        let strip_h_u = STATUSLINE_HEIGHT.min(height);
        let strip_y = height.saturating_sub(strip_h_u);
        if let Some(strip_h_nz) = NonZeroU32::new(strip_h_u) {
            damage.push(softbuffer::Rect {
                x: 0,
                y: strip_y,
                width: nz_w,
                height: strip_h_nz,
            });
        }

        // Tab strip damage rect.
        let tab_h_u = TAB_STRIP_HEIGHT.min(height.saturating_sub(strip_h_u));
        if let Some(tab_h_nz) = NonZeroU32::new(tab_h_u) {
            damage.push(softbuffer::Rect {
                x: 0,
                y: tab_y,
                width: nz_w,
                height: tab_h_nz,
            });
        }

        // Permissions prompt damage rect.
        if self.permissions_prompt.is_some() {
            let prompt_h_u = PERMISSIONS_PROMPT_HEIGHT.min(height.saturating_sub(prompt_y));
            if let Some(prompt_h_nz) = NonZeroU32::new(prompt_h_u) {
                damage.push(softbuffer::Rect {
                    x: 0,
                    y: prompt_y,
                    width: nz_w,
                    height: prompt_h_nz,
                });
            }
        }

        // Download notice damage rect.
        if current_notice.is_some() {
            let notice_h_u = DOWNLOAD_NOTICE_HEIGHT.min(height.saturating_sub(notice_y));
            if let Some(notice_h_nz) = NonZeroU32::new(notice_h_u) {
                damage.push(softbuffer::Rect {
                    x: 0,
                    y: notice_y,
                    width: nz_w,
                    height: notice_h_nz,
                });
            }
        }

        // Overlay popup — floats over the CEF region at top 1/3 of the window.
        // CEF is never resized when the overlay opens or closes.
        if let Some(overlay) = self.overlay.as_ref() {
            let bar = overlay.input();
            let popup_w = ((width * 60) / 100).clamp(200, OMNIBAR_POPUP_MAX_WIDTH);
            let popup_x = (width - popup_w) / 2;
            let popup_y = height / 3;
            // Add border so the inner rect always has room for the
            // input row (paint_at bails when inner_h < INPUT_HEIGHT,
            // which previously hid the input on empty omnibar).
            let popup_h =
                (bar.total_height() + 2 * OMNIBAR_POPUP_BORDER).min(height.saturating_sub(popup_y));

            // Border fill.
            fill_rect_u32(
                buf.as_mut(),
                width as usize,
                height as usize,
                popup_x as usize,
                popup_y as usize,
                popup_w as usize,
                popup_h as usize,
                OMNIBAR_POPUP_BORDER_COLOR,
            );
            // Inner background.
            let inner_x = popup_x + OMNIBAR_POPUP_BORDER;
            let inner_y = popup_y + OMNIBAR_POPUP_BORDER;
            let inner_w = popup_w.saturating_sub(2 * OMNIBAR_POPUP_BORDER);
            let inner_h = popup_h.saturating_sub(2 * OMNIBAR_POPUP_BORDER);
            fill_rect_u32(
                buf.as_mut(),
                width as usize,
                height as usize,
                inner_x as usize,
                inner_y as usize,
                inner_w as usize,
                inner_h as usize,
                OMNIBAR_POPUP_BG,
            );
            // Input bar + suggestions inside the popup.
            bar.paint_at(
                buf.as_mut(),
                width as usize,
                height as usize,
                inner_x as usize,
                inner_y as usize,
                inner_w as usize,
                inner_h as usize,
            );
            // Damage only the popup region.
            if let (Some(w_nz), Some(h_nz)) = (NonZeroU32::new(popup_w), NonZeroU32::new(popup_h)) {
                damage.push(softbuffer::Rect {
                    x: popup_x,
                    y: popup_y,
                    width: w_nz,
                    height: h_nz,
                });
            }
        }

        if let Err(err) = buf.present_with_damage(&damage) {
            warn!(error = %err, "softbuffer present_with_damage failed");
        }
    }

    /// Compute the CEF child window rect for the current overlay state.
    ///
    /// Vertical layout (top → bottom):
    ///
    /// 1. Download notice strip (`DOWNLOAD_NOTICE_HEIGHT`, when queued)
    /// 2. Permissions prompt (`PERMISSIONS_PROMPT_HEIGHT`, when active)
    /// 3. Tab strip (always, `TAB_STRIP_HEIGHT` px)
    /// 4. CEF page area  ← overlay floats *over* this, no resize on toggle
    /// 5. Statusline (always, `STATUSLINE_HEIGHT` px)
    fn cef_child_rect(&self, full_w: u32, full_h: u32) -> (u32, u32, u32, u32) {
        let status_h = STATUSLINE_HEIGHT.min(full_h);
        let remaining_after_status = full_h.saturating_sub(status_h);
        let tab_h = TAB_STRIP_HEIGHT.min(remaining_after_status);
        let remaining_after_tabs = remaining_after_status.saturating_sub(tab_h);
        let prompt_h = if self.permissions_prompt.is_some() {
            PERMISSIONS_PROMPT_HEIGHT.min(remaining_after_tabs)
        } else {
            0
        };
        let remaining_after_prompt = remaining_after_tabs.saturating_sub(prompt_h);
        let notice_h = if buffr_core::download_notice_queue_len(&self.download_notice_queue) > 0 {
            DOWNLOAD_NOTICE_HEIGHT.min(remaining_after_prompt)
        } else {
            0
        };
        let remaining_after_notice = remaining_after_prompt.saturating_sub(notice_h);
        let cef_w = full_w.max(1);
        let cef_h = remaining_after_notice.max(1);
        let cef_y = notice_h + prompt_h + tab_h;
        (0, cef_y, cef_w, cef_h)
    }

    /// The pixel row at which the tab strip begins (top of the
    /// `TAB_STRIP_HEIGHT` band). Mirrors the math in
    /// [`Self::cef_child_rect`] without depending on the CEF rect
    /// itself. The overlay is a floating popup and does not affect
    /// the tab strip position.
    fn tab_strip_y(&self, full_h: u32) -> u32 {
        let notice_h = if buffr_core::download_notice_queue_len(&self.download_notice_queue) > 0 {
            DOWNLOAD_NOTICE_HEIGHT
        } else {
            0
        };
        let prompt_h = if self.permissions_prompt.is_some() {
            PERMISSIONS_PROMPT_HEIGHT
        } else {
            0
        };
        (notice_h + prompt_h).min(full_h)
    }

    /// Top-of-window y for the download notice strip. Sits at the
    /// top of the window above the permissions prompt. The overlay is
    /// a floating popup and does not affect this position.
    fn download_notice_y(&self) -> u32 {
        0
    }

    /// Top-of-window y for the permissions prompt strip. Sits right
    /// below the download notice (when active) and above the tab strip.
    /// The overlay is a floating popup and does not affect this position.
    fn permissions_prompt_y(&self) -> u32 {
        if buffr_core::download_notice_queue_len(&self.download_notice_queue) > 0 {
            DOWNLOAD_NOTICE_HEIGHT
        } else {
            0
        }
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
        // Overlay is a floating popup — no CEF resize on toggle.
        self.request_redraw();
    }

    fn open_omnibar(&mut self) {
        let mut bar = InputBar::with_prefix("> ");
        // Pre-populate with the current page URL so the user can edit
        // it in place — Vimium / qutebrowser convention. Internal
        // buffr:// URLs (new-tab page, etc.) start empty so the user
        // can type a fresh query immediately.
        let url = &self.statusline.url;
        if !url.starts_with("buffr:") {
            bar.buffer = url.clone();
            bar.cursor = bar.buffer.len();
        }
        self.overlay = Some(OverlayState::Omnibar(bar));
        self.refresh_overlay_suggestions();
        // Overlay is a floating popup — no CEF resize on toggle.
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
        self.request_redraw();
    }

    fn close_overlay(&mut self) {
        if self.overlay.is_none() {
            return;
        }
        self.overlay = None;
        // Engine flips back to Normal so the modal trie resumes.
        if let Ok(mut e) = self.engine.lock() {
            e.set_mode(PageMode::Normal);
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
            OverlayState::Find { .. } => Vec::new(),
        };
        // Re-borrow the overlay since `self.command_suggestions` /
        // `omnibar_suggestions` need `&self`.
        if let Some(overlay) = self.overlay.as_mut() {
            overlay.input_mut().set_suggestions(suggestions);
        }
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
        true
    }

    fn confirm_overlay(&mut self) {
        let Some(overlay) = self.overlay.take() else {
            return;
        };
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
                            tracing::info!(%url, ?tags, "bookmark added");
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
                    tracing::info!(
                        %field_id,
                        ?kind,
                        already_editing,
                        "drain_edit_focus_events: Focus received"
                    );
                    if !already_editing {
                        if let Some(host) = self.host.as_ref() {
                            host.run_edit_attach(&field_id);
                        }
                        if let Ok(mut e) = self.engine.lock() {
                            e.set_mode(buffr_modal::PageMode::Insert);
                        }
                        tracing::info!(%field_id, "edit-mode entered");
                        self.edit_focus = EditFocus::Editing { field_id };
                        mode_changed = true;
                    }
                }
                EditConsoleEvent::Blur { field_id } => {
                    let matches_current = match &self.edit_focus {
                        EditFocus::Editing { field_id: f } => *f == field_id,
                        EditFocus::None => false,
                    };
                    if matches_current {
                        self.edit_focus = EditFocus::None;
                        if let Ok(mut e) = self.engine.lock() {
                            e.set_mode(buffr_modal::PageMode::Normal);
                        }
                        mode_changed = true;
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
            }
        }
        if mode_changed {
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
        tracing::info!(
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
                host.run_js(
                    "(function(){var el=document.activeElement;if(!el)return;\
        var k={key:'Escape',code:'Escape',keyCode:27,which:27,bubbles:true,cancelable:true};\
        el.dispatchEvent(new KeyboardEvent('keydown',k));\
        el.dispatchEvent(new KeyboardEvent('keyup',k));\
        el.blur();})();",
                );
            }
            if let Ok(mut e) = self.engine.lock() {
                e.set_mode(PageMode::Normal);
            }
            self.refresh_title();
            self.request_redraw();
            tracing::info!("edit_mode: exited via Esc — engine=Normal, edit_focus=None");
            return true;
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
        true
    }

    /// Resolve the front-of-queue permission with `outcome`. The
    /// callback fires exactly once; the next prompt (if any) is
    /// drawn on the following tick via [`Self::sync_permissions_prompt`].
    fn resolve_permission(&mut self, outcome: PromptOutcome) {
        let Some(pending) = pop_permission_front(&self.permissions_queue) else {
            warn!("permissions: resolve called with empty queue");
            self.permissions_prompt = None;
            return;
        };
        if let Err(err) = pending.resolve(outcome, &self.permissions) {
            warn!(error = %err, "permissions: resolve failed");
        }
        self.permissions_prompt = None;
        // Pull the next prompt immediately so the chrome shows it
        // without waiting for the next tick.
        self.sync_permissions_prompt();
        self.resync_cef_rect();
        self.request_redraw();
    }

    /// Route a keystroke to the active permission prompt. Returns
    /// `true` when the key was consumed.
    ///
    /// Bindings:
    ///
    /// - `a` / `y` — allow once
    /// - `A` / `Y` — allow + remember
    /// - `d` / `n` — deny once
    /// - `D` / `N` — deny + remember
    /// - `s` — deny + remember (qutebrowser parity for "stop")
    /// - `Esc` — defer (deny once, no persistence)
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
}

// ---- OSR input helpers ---------------------------------------------------

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

/// Omnibar / command-line popup geometry.
const OMNIBAR_POPUP_MAX_WIDTH: u32 = 800;
const OMNIBAR_POPUP_BORDER: u32 = 2;
const OMNIBAR_POPUP_BG: u32 = 0x1a1b26;
const OMNIBAR_POPUP_BORDER_COLOR: u32 = 0x7aa2f7;

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
    for row in y..y1 {
        let base = row * buf_w;
        for col in x..x1 {
            if let Some(px) = buf.get_mut(base + col) {
                *px = color;
            }
        }
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

impl ApplicationHandler for AppState {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let win_attrs = Window::default_attributes()
            .with_title(self.title_for(self.current_mode_label))
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

        // CEF child window leaves room at the bottom for the chrome
        // strip. We pass the trimmed size so the X11 child rect is
        // sized correctly from frame zero.
        let inner = window.inner_size();
        let chrome_h = STATUSLINE_HEIGHT.min(inner.height);
        let cef_w = inner.width.max(1);
        let cef_h = inner.height.saturating_sub(chrome_h).max(1);

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
                info!(
                    url = %self.homepage,
                    mode = ?host.mode(),
                    "browser host created"
                );
                // CEF stays focused for the lifetime of the browser
                // so DOM clicks deliver focus to inputs. We do NOT
                // forward OS-level Focused(false) (alt-tab) so pages
                // retain state. Insert mode transitions are tracked
                // independently via the modal engine.
                host.osr_focus(true);
                self.host = Some(host);
            }
            Err(err) => {
                warn!(error = %err, "failed to create browser host");
            }
        }

        // softbuffer context lives off the display handle; surface
        // wraps the window. Both must outlive any `buffer_mut()` call.
        match window.display_handle() {
            Ok(_) => {
                let context = match softbuffer::Context::new(window.clone()) {
                    Ok(c) => c,
                    Err(err) => {
                        warn!(error = %err, "softbuffer Context::new failed");
                        self.window = Some(window);
                        return;
                    }
                };
                let surface = match softbuffer::Surface::new(&context, window.clone()) {
                    Ok(s) => s,
                    Err(err) => {
                        warn!(error = %err, "softbuffer Surface::new failed");
                        self.softbuffer_ctx = Some(context);
                        self.window = Some(window);
                        return;
                    }
                };
                self.softbuffer_ctx = Some(context);
                self.softbuffer_surface = Some(surface);
            }
            Err(err) => warn!(error = %err, "no raw display handle for softbuffer"),
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
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => {
                info!("close requested");
                self.save_session_now();
                event_loop.exit();
            }
            WindowEvent::RedrawRequested => {
                self.paint_chrome();
            }
            WindowEvent::Resized(new_size) => {
                // Trim the CEF child to leave room for the chrome
                // strips. `cef_child_rect` accounts for the overlay
                // when active.
                let (_x, _y, cef_w, cef_h) =
                    self.cef_child_rect(new_size.width.max(1), new_size.height.max(1));
                if let Some(host) = self.host.as_ref() {
                    match host.mode() {
                        buffr_core::HostMode::Windowed => {
                            host.resize(cef_w, cef_h);
                        }
                        buffr_core::HostMode::Osr => {
                            // OSR: update viewport atomics + trigger was_resized()
                            // so CEF re-paints at the new dimensions.
                            host.osr_resize(cef_w, cef_h);
                        }
                    }
                }
                self.request_redraw();
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
                    let mods = winit_mods_to_cef(&self.modifiers);
                    host.osr_mouse_move(bx, by, mods);
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                use winit::event::{ElementState::Pressed, MouseButton};
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

                // Tab-strip click: Left = focus, Middle = close. Both
                // on press. Intercept before OSR.
                if state == Pressed
                    && (button == MouseButton::Left || button == MouseButton::Middle)
                    && let Some(window) = self.window.as_ref()
                {
                    let size = window.inner_size();
                    let full_w = size.width.max(1);
                    let full_h = size.height.max(1);
                    let tab_y = self.tab_strip_y(full_h);
                    let tab_y_end = tab_y + TAB_STRIP_HEIGHT;
                    // osr_cursor is in browser-region coords:
                    //   bx = position.x, by = position.y - cef_y
                    // So window_y = osr_cursor.1 + cef_y.
                    let (_, cef_y, _, _) = self.cef_child_rect(full_w, full_h);
                    let wx = self.osr_cursor.0 as u32;
                    let wy = (self.osr_cursor.1 + cef_y as i32).max(0) as u32;
                    if wy >= tab_y && wy < tab_y_end && !self.tab_ids.is_empty() {
                        // Compute tab width using same algorithm as TabStrip::paint.
                        let n = self.tab_ids.len() as u32;
                        const GUTTER: u32 = 4;
                        let available = full_w.saturating_sub((n + 1) * GUTTER);
                        let raw_w = available / n.max(1);
                        let tab_w = raw_w.clamp(buffr_ui::MIN_TAB_WIDTH, buffr_ui::MAX_TAB_WIDTH);
                        // Tabs start at x = GUTTER, each occupies tab_w + GUTTER.
                        if wx >= GUTTER {
                            let rel_x = wx - GUTTER;
                            let idx = (rel_x / (tab_w + GUTTER)) as usize;
                            if idx < self.tab_ids.len() {
                                let id = self.tab_ids[idx];
                                if button == MouseButton::Middle {
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
                                } else if let Some(host) = self.host.as_ref() {
                                    host.select_tab(id);
                                }
                                return;
                            }
                        }
                    }
                }

                if let Some(host) = self.host.as_ref()
                    && host.mode() == buffr_core::HostMode::Osr
                    && let Some(cef_button) = winit_button_to_cef(&button)
                {
                    let mouse_up = state == winit::event::ElementState::Released;
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
                    }
                    let mods = winit_mods_to_cef(&self.modifiers);
                    let (bx, by) = self.osr_cursor;
                    host.osr_mouse_click(bx, by, cef_button, mouse_up, self.osr_click_count, mods);
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if let Some(host) = self.host.as_ref()
                    && host.mode() == buffr_core::HostMode::Osr
                {
                    use winit::event::MouseScrollDelta;
                    let (dx, dy) = match delta {
                        // Line delta: 120 per tick matches Chromium's wheel
                        // tick magnitude expectation.
                        MouseScrollDelta::LineDelta(x, y) => {
                            ((x * 120.0) as i32, (y * 120.0) as i32)
                        }
                        MouseScrollDelta::PixelDelta(p) => (p.x as i32, p.y as i32),
                    };
                    let mods = winit_mods_to_cef(&self.modifiers);
                    let (bx, by) = self.osr_cursor;
                    host.osr_mouse_wheel(bx, by, dx, dy, mods);
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
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
                let Some(chord) = key_event_to_chord(&event, self.modifiers) else {
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

        // Pump CEF every frame. With `ControlFlow::Poll` this fires
        // continuously, which is the simplest correct cadence for
        // Phase 1 — Phase 3 will switch to a tickless wakeup.
        cef::do_message_loop_work();

        // Edit-mode: drain focus/blur/mutate events from the JS bridge.
        // Runs before the engine tick so state is consistent when key
        // routing fires later in the same event-loop iteration.
        self.drain_edit_focus_events();

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

        // Drain any find result the CEF browser thread posted since
        // the last tick, then check whether the `--find` smoke
        // dispatch is due.
        self.pump_find_results();
        self.maybe_dispatch_find_smoke();

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
                self.request_redraw();
            }
        }

        // Permission prompt: pull the front of the queue into a
        // visible widget. `sync_permissions_prompt` is a no-op when a
        // prompt is already active, so the user always sees one
        // request at a time.
        if self.sync_permissions_prompt() {
            self.resync_cef_rect();
            self.request_redraw();
        }

        // Live URL sync: poll the active tab's main-frame URL each tick
        // and push it into the statusline. Cheap (one CEF call + string
        // compare); redraw only on change.
        if let Some(host) = self.host.as_ref() {
            let live = host.active_tab_live_url();
            if !live.is_empty() && live != self.statusline.url {
                self.statusline.url = live;
                self.request_redraw();
            }
        }

        // Download notices: drop any that have lived past their expiry
        // window. Trigger a redraw + resync when the queue changes so
        // the chrome immediately reclaims the strip height.
        {
            let dropped = expire_stale_notices(&self.download_notice_queue);
            if dropped > 0 {
                self.resync_cef_rect();
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
                self.request_redraw();
            }
        }

        // OSR poll-redraw throttle — ~60 Hz.
        //
        // In a complete pipeline, `OsrPaintHandler::on_paint` would post a
        // wakeup through an `EventLoopProxy` and we'd only redraw when CEF
        // delivers a new frame.  That signal channel is not yet wired up
        // (deferred to step 5 / a follow-up).  Until then we request a redraw
        // at ≈60 Hz whenever the host is in OSR mode so the page stays live.
        // This only fires for Wayland sessions; X11 windowed mode is unaffected.
        //
        // TODO: replace with EventLoopProxy-based wakeup from on_paint.
        if let Some(host) = self.host.as_ref()
            && host.mode() == buffr_core::HostMode::Osr
        {
            let now = Instant::now();
            if now.duration_since(self.last_osr_redraw) >= Duration::from_millis(16) {
                self.last_osr_redraw = now;
                self.request_redraw();
            }
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
