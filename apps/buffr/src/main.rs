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
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use buffr_config::{ClearableData, Config, ConfigSource};
use buffr_core::cmdline::{Command, parse as parse_cmdline};
use buffr_core::{
    BuffrApp, FindResultSink, HintAction, HintAlphabet, HintEventSink, init_cef_api, new_find_sink,
    new_hint_event_sink, profile_paths,
};
use buffr_modal::{Engine, Key, NamedKey, PageMode, Step, key_event_to_chord};
use buffr_ui::{
    CertState, FindStatus, HintStatus as UiHintStatus, InputBar, STATUSLINE_HEIGHT, Statusline,
    Suggestion, SuggestionKind, TAB_STRIP_HEIGHT, TabStrip, TabView,
};

mod session;
use cef::{ImplBrowser, Settings};
use clap::Parser;
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use tempfile::TempDir;
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

    // -------- history store --------
    //
    // Phase 5: SQLite-backed history at
    // `<data>/history.sqlite`. `BrowserHost` keeps an `Arc<History>`
    // and CEF's `LoadHandler` / `DisplayHandler` (wired in
    // `buffr_core::handlers`) pump every main-frame visit + title
    // into it. Private mode opens an in-memory DB instead.
    let history = Arc::new(if cli.private {
        buffr_history::History::open_in_memory().context("opening in-memory history")?
    } else {
        buffr_history::History::open(paths.data.join("history.sqlite"))
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

    let find_sink = new_find_sink();
    let hint_sink = new_hint_event_sink();
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

    let mut app_state = AppState::new(
        homepage,
        engine,
        history.clone(),
        bookmarks.clone(),
        downloads.clone(),
        downloads_config,
        zoom.clone(),
        search_config,
        cli.private,
        find_sink,
        hint_sink,
        hint_alphabet,
        cli.find.clone(),
        cli.new_tab.clone(),
        pending_session_tabs,
        session_path,
    );
    if let Err(err) = event_loop.run_app(&mut app_state) {
        warn!(error = %err, "winit event loop exited with error");
    }

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
}

/// Active overlay above the CEF child window.
///
/// Both variants wrap the same [`InputBar`]; the discriminant decides
/// which suggestion source to query and how to handle Enter. The
/// engine sits in [`PageMode::Command`] for both, so the discriminant
/// is the only way to tell them apart at dispatch time.
#[derive(Debug)]
enum OverlayState {
    Command(InputBar),
    Omnibar(InputBar),
}

impl OverlayState {
    fn input(&self) -> &InputBar {
        match self {
            OverlayState::Command(b) | OverlayState::Omnibar(b) => b,
        }
    }
    fn input_mut(&mut self) -> &mut InputBar {
        match self {
            OverlayState::Command(b) | OverlayState::Omnibar(b) => b,
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
        search_config: Arc<buffr_config::Search>,
        private: bool,
        find_sink: FindResultSink,
        hint_sink: HintEventSink,
        hint_alphabet: HintAlphabet,
        pending_find: Option<String>,
        pending_new_tabs: Vec<String>,
        pending_session_tabs: Vec<session::PersistedTab>,
        session_path: Option<PathBuf>,
    ) -> Self {
        let mut statusline = Statusline {
            url: homepage.clone(),
            private,
            cert_state: CertState::Unknown,
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
            search_config,
            overlay: None,
            private,
            modifiers: ModifiersState::empty(),
            startup: Instant::now(),
            current_mode_label: mode_label(PageMode::Normal),
            find_sink,
            hint_sink,
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
            A::DuplicateTab => {
                if let Err(err) = host.duplicate_active() {
                    warn!(error = %err, "duplicate_tab: failed");
                }
            }
            A::PinTab => host.toggle_pin_active(),
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
        let tabs = summaries
            .into_iter()
            .map(|t| TabView {
                title: t.title,
                progress: t.progress,
                pinned: t.pinned,
                private: t.private,
            })
            .collect();
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
        // softbuffer surface borrow — `tab_strip_y` needs `&self`
        // and we only release once the buffer drops.
        let tab_y = self.tab_strip_y(height);
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

        let mut damage = Vec::with_capacity(3);

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

        // Overlay damage rect (top, if active).
        if let Some(overlay) = self.overlay.as_ref() {
            let bar = overlay.input();
            bar.paint(buf.as_mut(), width as usize, height as usize);
            let overlay_h = bar.total_height().min(height);
            if let Some(h_nz) = NonZeroU32::new(overlay_h) {
                damage.push(softbuffer::Rect {
                    x: 0,
                    y: 0,
                    width: nz_w,
                    height: h_nz,
                });
            }
        }

        if let Err(err) = buf.present_with_damage(&damage) {
            warn!(error = %err, "softbuffer present_with_damage failed");
        }
    }

    /// Compute the CEF child window rect for the current overlay
    /// state. Vertical layout (top → bottom):
    ///
    /// 1. Input bar (when overlay open)
    /// 2. Tab strip (always, `TAB_STRIP_HEIGHT` px)
    /// 3. CEF page area
    /// 4. Statusline (always, `STATUSLINE_HEIGHT` px)
    fn cef_child_rect(&self, full_w: u32, full_h: u32) -> (u32, u32, u32, u32) {
        let status_h = STATUSLINE_HEIGHT.min(full_h);
        let remaining_after_status = full_h.saturating_sub(status_h);
        let tab_h = TAB_STRIP_HEIGHT.min(remaining_after_status);
        let remaining_after_tabs = remaining_after_status.saturating_sub(tab_h);
        let overlay_h = self
            .overlay
            .as_ref()
            .map(|o| o.input().total_height())
            .unwrap_or(0)
            .min(remaining_after_tabs);
        let cef_w = full_w.max(1);
        let cef_h = remaining_after_tabs.saturating_sub(overlay_h).max(1);
        let cef_y = overlay_h + tab_h;
        (0, cef_y, cef_w, cef_h)
    }

    /// The pixel row at which the tab strip begins (top of the
    /// `TAB_STRIP_HEIGHT` band). Mirrors the math in
    /// [`Self::cef_child_rect`] without depending on the CEF rect
    /// itself.
    fn tab_strip_y(&self, full_h: u32) -> u32 {
        let overlay_h = self
            .overlay
            .as_ref()
            .map(|o| o.input().total_height())
            .unwrap_or(0);
        overlay_h.min(full_h)
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
        self.resync_cef_rect();
        self.request_redraw();
    }

    fn open_omnibar(&mut self) {
        let mut bar = InputBar::with_prefix("> ");
        // Pre-populate with the current page URL so the user can edit
        // it in place — Vimium / qutebrowser convention.
        bar.buffer = self.statusline.url.clone();
        bar.cursor = bar.buffer.len();
        self.overlay = Some(OverlayState::Omnibar(bar));
        self.refresh_overlay_suggestions();
        self.resync_cef_rect();
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
        self.resync_cef_rect();
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
        let chord = match key_event_to_chord(event, self.modifiers) {
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
                        Ok(_) => tracing::info!(%url, ?tags, "bookmark added"),
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

    fn dispatch_omnibar(&mut self, bar: &InputBar) {
        let raw = bar.current_value().to_string();
        if raw.is_empty() {
            return;
        }
        // If a suggestion is selected its `value` is already a real
        // URL; otherwise resolve the typed buffer.
        let target = if bar.selected.is_some() {
            raw
        } else {
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

        match buffr_core::BrowserHost::new(
            raw,
            &self.homepage,
            self.history.clone(),
            self.downloads.clone(),
            self.downloads_config.clone(),
            self.zoom.clone(),
            self.find_sink.clone(),
            self.hint_sink.clone(),
            self.hint_alphabet.clone(),
            (cef_w, cef_h),
        ) {
            Ok(host) => {
                info!(url = %self.homepage, "browser host created");
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
                    host.resize(cef_w, cef_h);
                }
                self.request_redraw();
            }
            WindowEvent::ModifiersChanged(mods) => {
                self.modifiers = mods.state();
            }
            WindowEvent::KeyboardInput { event, .. } => {
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
}
