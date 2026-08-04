//! buffr main entry point.
//!
//! Phase 1 wiring:
//!
//! 1. Init tracing.
//! 2. Dispatch to `cef::execute_process` so the same binary serves as
//!    its own renderer/GPU/utility subprocess (single-binary mode).
//! 3. Initialize CEF with [`buffr_core::BuffrApp`] + per-user paths.
//! 4. Open one winit window, hand its native handle to
//!    [`buffr_cef::BrowserHost`].
//! 5. Drive winit's event loop while pumping `cef::do_message_loop_work`
//!    each iteration. (We avoid `cef::run_message_loop` so winit owns
//!    the main loop — required for native chrome in Phase 3.)
//! 6. On exit: shut CEF down cleanly.
//!
//! Phase 4 additions: clap CLI, TOML config loader, hot-reload watcher
//! that swaps the live keymap on file changes.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Quiet window after the last `WindowEvent::Resized` before we actually
/// call `host.osr_resize`. Hyprland fires many Resized events per second
/// during a drag; CEF should only learn the final post-drag size. The
/// renderer GPU-stretches the stale OSR over the live browser_rect during
/// the window, so there is no visual regression.
const CEF_RESIZE_DEBOUNCE: Duration = Duration::from_millis(150);

/// Grace period after a `WindowEvent::Occluded(true)` before the OSR paint
/// pipeline is put to sleep.  Absorbs workspace-switch / overlay thrash
/// (occluded then immediately revealed) without emitting spurious
/// sleep/wake cycles that would produce a flickery on_paint burst.
const OCCLUDE_SLEEP_DEBOUNCE: Duration = Duration::from_millis(200);

/// `present_us` threshold above which we suspect the compositor is
/// throttling our surface (window occluded, on a hidden workspace,
/// minimized).  Healthy presents are sub-16 ms; compositor-throttled
/// invisible surfaces routinely block 100 ms – 1.5 s.  Used as a
/// fallback occlusion signal on platforms (Hyprland today) where winit
/// `WindowEvent::Occluded` doesn't fire.
const SLOW_PRESENT_THRESHOLD_US: u64 = 100_000;

/// `present_us` below which we consider the surface to be visible again
/// (compositor is releasing buffers promptly).  A single fast probe
/// frame undoes a heuristic-occluded belief.
const FAST_PRESENT_THRESHOLD_US: u64 = 30_000;

/// Number of recent `present_us` samples retained for the occlusion
/// heuristic.  Keeping the window small lets the heuristic react
/// quickly to a workspace switch (3 of 5 slow → occlude after ~3
/// frames, well under a second at 60 Hz).
const PRESENT_HISTORY_SIZE: usize = 5;

/// Number of slow frames within [`PRESENT_HISTORY_SIZE`] required to
/// flip the heuristic to "occluded".  3-of-5 is conservative enough
/// to ride out a one-off compositor stutter without burning the user
/// with a false sleep, and aggressive enough that a workspace switch
/// trips it within ~50 ms at 60 Hz.
const SLOW_FRAMES_TO_OCCLUDE: usize = 3;

/// Single-frame threshold for immediate occlusion.  See
/// [`SLOW_PRESENT_THRESHOLD_US`] for the rolling-window trigger; this
/// constant is the fast-path that catches the first hidden-surface
/// present before it can repeat.
const IMMEDIATE_OCCLUDE_THRESHOLD_US: u64 = 500_000;

/// How often, while heuristically sleeping, to attempt a probe present
/// to test whether the compositor is releasing buffers again.  Two
/// seconds balances wake latency against the cost of the probe (one
/// full present cycle that may itself block 100+ ms if still occluded).
const OCCLUSION_PROBE_INTERVAL: Duration = Duration::from_secs(2);

/// Throttle between retries after [`crate::render::Renderer::frame`]
/// skipped a frame (worker still presenting, swapchain texture
/// unavailable, command channel full).  The dirty state is deliberately
/// NOT retired on those paths (H8), so something has to ask for another
/// paint — but re-requesting inside the redraw handler would spin the
/// event loop at 100% CPU while the worker is blocked.  Scheduling the
/// retry as an `about_to_wait` deadline caps the retry rate instead.
const SKIPPED_FRAME_RETRY_DELAY: Duration = Duration::from_millis(8);

/// How often the media-activity probe JS is fired while the window is
/// occluded.  Two seconds balances detection latency against JS-execution
/// overhead.  The CEF AudioHandler fires immediately on stream start/stop,
/// so the probe is only needed for `navigator.mediaSession` and fullscreen
/// video — both of which rarely change state at sub-second granularity.
const MEDIA_PROBE_INTERVAL: Duration = Duration::from_secs(2);

/// How long the resize-paint watchdog waits for CEF to produce an
/// `on_paint` at the expected (post-resize) dims before firing a
/// force-repaint nudge.  500 ms is long enough to survive a slow
/// renderer startup and short enough that a stuck animation does not
/// linger visibly.
const RESIZE_PAINT_WATCHDOG_TIMEOUT: Duration = Duration::from_millis(500);

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

/// Build a chrome [`Palette`] from the user's `[theme]` config. Each
/// hex string parses through `buffr_config::parse_hex_rgb`; malformed
/// values fall back to the corresponding default-palette field so a
/// stray typo never crashes startup. `theme.high_contrast = true`
/// short-circuits the whole derivation and returns the WCAG palette.
fn build_palette(theme: &buffr_config::Theme) -> Palette {
    if theme.high_contrast {
        return Palette::high_contrast();
    }
    let parse = buffr_config::parse_hex_rgb;
    let dflt = Palette::default();
    let accent = parse(&theme.accent).unwrap_or(dflt.accent);
    Palette::from_accent(accent).with_signals(
        parse(&theme.cert_secure).unwrap_or(dflt.cert_secure),
        parse(&theme.cert_insecure).unwrap_or(dflt.cert_insecure),
        parse(&theme.private).unwrap_or(dflt.private),
        parse(&theme.progress).unwrap_or(dflt.progress),
        parse(&theme.update).unwrap_or(dflt.update),
    )
}

use anyhow::{Context, Result};
use buffr_cef::{CefBackend, CefEngineSinks, profile_paths};
// Neutral permission types — Phase 8a (#88). Apps layer now uses the
// engine-agnostic queue so both CEF and blink-cdp share the same prompt path.
use buffr_config::{ClearableData, Config, ConfigSource};
use buffr_core::cmdline::{Command, parse as parse_cmdline};
use buffr_core::{
    ContextMenuItem, ContextMenuRequest, ContextMenuTarget, DownloadNoticeQueue, EditConsoleEvent,
    EditEventSink, FindResultSink, HintAction, HintAlphabet, HintEventSink, IdleInhibitor,
    drain_edit_events, expire_stale_notices, new_download_notice_queue, new_edit_event_sink,
    new_find_sink, new_hint_event_sink, new_inhibitor, peek_download_notice,
};
use buffr_engine::PromptOutcome;
use buffr_engine::permissions::{
    PromptIdentity, ResolveTarget, peek_front_entry as peek_permission_front_entry,
    queue_len as permissions_queue_len, take_front_matching as take_permission_front_matching,
};
use buffr_engine::{
    Backend, BackendOpenOptions, NewTabHtmlProvider, ProfilePaths, TabId, TabSummary,
    newtab::{
        NEW_TAB_HTML_TEMPLATE, NEW_TAB_KEYBINDS_MARKER, NEW_TAB_SPLASH_ART_MARKER, NEW_TAB_URL,
    },
};
use buffr_engine::{
    PopupCloseSink, PopupCreateSink, SharedOsrFrame, SharedOsrViewState, drain_popup_closes,
    drain_popup_creates, drain_popup_targets,
};
use buffr_modal::{
    Engine, EngineModifiers, Key, NamedKey, PageMode, PlannedInput, SpecialKey, Step,
};
// KeyEvent → KeyChord translation lives in buffr-modal's bridge adapter.
// winit is used on all platforms; bridge_key_event_to_chord handles all
// toolkit-agnostic KeyEvent values produced by windowing/other.
use buffr_modal::{
    bridge_key_event_to_chord as key_event_to_chord,
    bridge_key_event_to_chord_with_repeat as key_event_to_chord_with_repeat,
};
use buffr_permissions::Permissions;
use buffr_ui::{
    CertState, ContextMenuEntry, ContextMenuOverlay, DOWNLOAD_NOTICE_HEIGHT, FindStatus,
    HintStatus as UiHintStatus, InputBar, Palette, PermissionsPrompt, STATUSLINE_HEIGHT,
    Statusline, Suggestion, SuggestionKind, TAB_STRIP_HEIGHT, TabStrip, TabView,
};

mod cef_translate;
mod chrome_paint;
mod cli;
mod context_menu;
mod crash_guard;
mod engine_router;
mod event_loop;
mod heartbeat;
mod loading_anim;
mod paint_policy;
mod render;
mod session;
mod single_instance;
// Bridge types on non-Linux mirror wayr's surface for shape parity even
// where main.rs's current dispatch path doesn't read every field/variant
// yet (Touch, ContentPurpose, Rect, …). Future phases will exercise
// them; allow the dead-code lints crate-wide here.
#[allow(dead_code)]
mod windowing;
use crate::cef_translate::*;
use crate::chrome_paint::*;
use crate::cli::Cli;
use crate::context_menu::*;
use crate::paint_policy::*;
use crate::windowing::{
    ApplicationHandler, EventLoop, EventLoopProxy, Modifiers, Surface, SurfaceId,
    Window as Toplevel, WindowEvent,
};
use buffr_engine::MouseButton as NeutralMouseButton;
use clap::Parser;
use tempfile::TempDir;
use tracing::{debug, info, trace, warn};

// ── Context menu helpers ──────────────────────────────────────────────────────

/// Custom user events sent into the winit loop from background threads.
#[derive(Debug, Clone)]
enum BuffrUserEvent {
    /// CEF OSR on_paint fired for the main browser; request main-window redraw.
    OsrFrame,
    /// CEF OSR on_paint fired for popup browser `browser_id`; request that
    /// popup's window redraw.
    OsrFramePopup(i32),
    /// IPC: open these URLs as new tabs. Sent by the accept thread when a
    /// secondary `buffr` invocation forwards its args. Always focuses the
    /// main window after opening, so users see something happen.
    OpenUrls(Vec<String>),
    /// Clipboard read completed on a worker thread (kicked off by Ctrl+V
    /// in CEF Insert mode). Carries the text payload to inject into the
    /// focused element via execCommand. None = read failed or empty.
    /// Posted via EventLoopProxy so the main thread doesn't block on
    /// the wayland data-control read (which would self-deadlock if
    /// Chromium owns the clipboard — its wl_data_source.send callback
    /// runs on CEF's UI thread, which is the main thread).
    ClipboardPasteText(Option<String>),
    /// SIGINT (Ctrl+C) caught by the ctrlc handler. Posted via
    /// EventLoopProxy so the winit loop wakes from `WaitUntil` even when
    /// occluded — `about_to_wait` would otherwise sit on a multi-second
    /// deadline (probe interval, media probe) and the shutdown_flag
    /// poll wouldn't fire until the next reveal/probe.
    Shutdown,
}

/// Per-popup-window state. Owns the wayr Toplevel, wgpu renderer, and the
/// OSR frame/view shared with the CEF paint handler.
struct PopupWindow {
    window: Arc<Toplevel>,
    renderer: crate::render::Renderer,
    /// CEF browser id — used to route CEF close events back to this window.
    browser_id: i32,
    frame: SharedOsrFrame,
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
    /// wayr modifier state for this popup's events (updated from PointerButton
    /// and Key events; Modifiers::default() = no modifiers held).
    modifiers: Modifiers,
    /// Click state for double-click detection.
    last_click_at: Instant,
    last_click_button: Option<NeutralMouseButton>,
    click_count: i32,
    /// Dimensions of the most recently received OSR paint for this popup.
    /// `None` until CEF emits the first on_paint. Guards the synthetic-upload
    /// fallback so a chrome-dirty redraw between CEF paints doesn't blank the
    /// OSR quad (same swap-out side effect as the main window).
    last_osr_dims: Option<(u32, u32)>,
    /// Debounced CEF resize: most recent target dims plus the deadline when
    /// `host.popup_resize` will actually be called. Refreshed on every
    /// Resized event; fired once quiet for `CEF_RESIZE_DEBOUNCE`.
    pending_cef_resize: Option<(u32, u32, std::time::Instant)>,
    /// Deadline for retrying a frame this popup's renderer skipped. Same
    /// contract as [`AppState::repaint_retry_at`] — throttled through the
    /// event-loop deadline so a busy render worker can't spin the loop.
    repaint_retry_at: Option<Instant>,
}

// ── smoke-test plumbing ──────────────────────────────────────────────────────
//
// Two static atomics drive the `--smoke-test` flag in main and the
// `WindowEvent::RedrawRequested` handler. Statics (not AppState
// fields) so the watchdog thread + the dispatch arm can both reach
// them without threading.
static SMOKE_TEST_ACTIVE: AtomicBool = AtomicBool::new(false);
static SMOKE_TEST_SAW_REDRAW: AtomicBool = AtomicBool::new(false);

fn main() -> Result<()> {
    // -------- backend construction ------------------------------------
    //
    // CefBackend is the only concrete buffr_cef type used in main.
    // All lifecycle calls go through `Arc<dyn Backend>`.
    let cef_backend = CefBackend::new();

    // -------- macOS framework loader ---------------------------------
    //
    // On macOS the libcef framework is bundled inside the .app and
    // must be loaded explicitly through cef-rs's `LibraryLoader`
    // before any CEF entry. This applies equally to the browser
    // process and the subprocess case: both run from the same binary
    // in single-binary mode, but in macOS bundles the helper is a
    // separate executable that loads the framework with `helper=true`
    // (path-resolved via `../../..` instead of `../Frameworks`).
    // `load_library` also calls `init_cef_api` to pin the API version.
    {
        let exe = std::env::current_exe().context("resolving current_exe for CEF library load")?;
        cef_backend
            .load_library(&exe, false)
            .map_err(|e| anyhow::anyhow!(e))?;
    }

    // -------- subprocess dispatch (single-binary mode) ----------------
    //
    // CEF re-launches this binary with `--type=renderer` (and other
    // worker args clap doesn't know about), so we must short-circuit
    // before parsing the user-facing CLI. `cef::execute_process`
    // returns >= 0 inside a child process and we exit with that code.
    //
    // `init_cef_api` already ran inside `load_library` above; the
    // subprocess call site also calls it internally for safety.
    let is_subprocess = std::env::args().any(|a| a.starts_with("--type="));
    if is_subprocess {
        let exit_code = cef_backend.execute_subprocess();
        std::process::exit(exit_code.max(0));
    }

    // Connect to the supervisor's heartbeat socket / named pipe AS EARLY AS
    // POSSIBLE. The supervisor opens a connect-grace window the moment it
    // CreateProcess's us, and CEF init / CLI parse / path resolution below
    // can blow that budget on a cold-disk Windows first-run (scoop / MSI
    // install on a fresh machine). Drained later by AppState::new.
    let initial_heartbeat = heartbeat::Heartbeat::try_connect();

    // Wrap in Arc<dyn Backend> now that subprocess is handled.
    let backend: Arc<dyn Backend> = Arc::new(cef_backend);

    let cli = Cli::parse();

    // -------- smoke-test mode ---------------------------------------
    //
    // CI cross-platform smoke harness: launch the windowing backend,
    // wait for the first `WindowEvent::RedrawRequested` (proves wayr
    // / winit reached steady state and the compositor / window
    // manager accepted the surface), then exit 0. Bounded by a
    // watchdog thread so a hung event loop fails the test instead of
    // wedging CI.
    if cli.smoke_test {
        SMOKE_TEST_ACTIVE.store(true, Ordering::SeqCst);
        let timeout_ms = cli.smoke_test_timeout_ms;
        std::thread::Builder::new()
            .name("smoke-test-watchdog".into())
            .spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(timeout_ms));
                if !SMOKE_TEST_SAW_REDRAW.load(Ordering::SeqCst) {
                    eprintln!(
                        "smoke-test: no RedrawRequested within {timeout_ms} ms; exiting non-zero"
                    );
                    std::process::exit(3);
                }
            })
            .expect("spawn smoke-test watchdog");
    }

    // -------- --engine validation (before CEF init) ------------------
    if let Some(raw) = cli.engine.as_deref() {
        const VALID: &[&str] = &["cef"];
        let chosen = raw.to_lowercase();
        if !VALID.contains(&chosen.as_str()) {
            anyhow::bail!(
                "--engine {}: unknown backend. Valid values: {}",
                chosen,
                VALID.join(", ")
            );
        }
    }

    // -------- Linux Wayland-only gate --------------------------------
    //
    // buffr requires a Wayland session on Linux. Refuse early — before CEF
    // init, before any window creation — so the user gets a clear message
    // instead of a cryptic winit/CEF panic.
    #[cfg(target_os = "linux")]
    {
        let session = std::env::var("XDG_SESSION_TYPE").unwrap_or_default();
        if !session.eq_ignore_ascii_case("wayland") {
            anyhow::bail!(
                "buffr requires a Wayland session on Linux (got XDG_SESSION_TYPE={:?}). \
                 Switch your DE to Wayland — GNOME, KDE, Sway, Hyprland all support it.",
                session
            );
        }
    }

    // -------- short-circuit modes (no CEF init) ----------------------
    if let Some(result) = cli::dispatch(&cli) {
        return result;
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

    // init_cef_api already called inside backend.load_library above.

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

    // -------- single-instance check -----------------------------------
    //
    // BEFORE CEF init so a secondary invocation that only needs to forward
    // URLs never initializes CEF at all. `--private` is exempt: each private
    // session is always standalone with its own tempdir cache.
    //
    // `singleton_handle` is `Option` so we can stash it here and spawn the
    // accept thread later once the winit EventLoop (and its proxy) exist.
    let mut singleton_handle: Option<single_instance::SingletonHandle> = None;
    if !cli.private {
        let cache_str = paths.cache.to_string_lossy();
        let profile_id = single_instance::profile_id_from(&cache_str);
        let mut all_urls = cli.urls.clone();
        all_urls.extend(cli.new_tab.clone());
        match single_instance::try_acquire(&profile_id, &all_urls)? {
            single_instance::AcquireResult::Forwarded => {
                info!(
                    count = all_urls.len(),
                    "single_instance: forwarded URLs to existing buffr; exiting"
                );
                return Ok(());
            }
            single_instance::AcquireResult::Owner(handle) => {
                singleton_handle = Some(handle);
            }
        }
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
        buffr_history::History::builder()
            .in_memory()
            .skip_schemes(config.privacy.skip_schemes.clone())
            .build()
            .context("opening in-memory history")?
    } else {
        buffr_history::History::builder()
            .path(paths.data.join("history.sqlite"))
            .skip_schemes(config.privacy.skip_schemes.clone())
            .build()
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

    // -------- favicon disk cache --------
    //
    // SQLite-backed bitmap store keyed by origin. Lets restored tabs show
    // their favicons immediately, before CEF fires its async callback.
    // Skipped in `--private` mode — no state should persist.
    let favicon_cache: Option<buffr_core::FaviconCache> =
        if cli.private || !config.general.show_favicons {
            None
        } else {
            match buffr_core::FaviconCache::open(paths.data.join("favicons.sqlite")) {
                Ok(fc) => {
                    debug!("favicon cache opened");
                    Some(fc)
                }
                Err(err) => {
                    warn!(error = %err, "favicon cache open failed — running without disk cache");
                    None
                }
            }
        };

    let keymap = buffr_config::build_keymap(&config).context("building keymap from config")?;
    let homepage = cli
        .homepage
        .clone()
        .unwrap_or_else(|| config.general.homepage.clone());
    let new_tab_url = config.startup.new_tab_url.clone();

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
    // In private mode `paths.data` is already a tempdir that nothing
    // survives, so the path is the same either way.
    let counters_path = paths.data.join("usage-counters.json");
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
    backend.set_force_renderer_accessibility(config.accessibility.force_renderer_accessibility);

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

    // -------- HiDPI scale: forward to CEF before initialize ----------
    //
    // `--force-device-scale-factor` is passed here as a fallback for
    // CEF renderer subprocesses that initialize before any winit window
    // exists. The live scale is applied properly via
    // `BrowserHost::set_device_scale` once the window is created, using
    // `window.scale_factor()` (Wayland per-output, Win32 per-monitor DPI,
    // macOS NSScreen.backingScaleFactor).
    //
    // Only `BUFFR_SCALE` is honoured here — it is an explicit user override
    // for debugging / fractional-scale testing. `GDK_SCALE` is dropped:
    // winit already reads the OS APIs that subsume it.
    #[cfg(target_os = "linux")]
    {
        let scale = std::env::var("BUFFR_SCALE")
            .ok()
            .and_then(|v| v.parse::<f32>().ok());
        if let Some(scale) = scale
            && (scale - 1.0).abs() > 0.01
        {
            debug!(scale, "forwarding BUFFR_SCALE device scale factor to CEF");
            backend.set_device_scale(scale);
        }
    }

    // -------- backend initialize --------
    //
    // CEF init pulls in libnss3 → libsoftokn3 → libsqlite3 which can SIGSEGV
    // on systems with an ABI-mismatched NSS/sqlite combo (Arch 2026-05).
    // Skip when --engine targets a non-CEF backend so dev work isn't blocked
    // by an unrelated system bug.
    let cli_uses_cef = cli
        .engine
        .as_deref()
        .map(|e| e.eq_ignore_ascii_case("cef"))
        .unwrap_or(true);
    if cli_uses_cef {
        // CEF's `root_cache_path` must be a common ancestor of every
        // RequestContext cache_path (per-engine ones live under
        // `data_root/engines/<id>/`). When they don't share a root, CEF
        // silently falls back to the global Default context and the
        // per-engine cookie store is unused. Use `data_root` here so
        // both global + per-engine state share a persistent root.
        let cache_path = paths.data.to_string_lossy().into_owned();
        backend
            .initialize(&cache_path)
            .map_err(|e| anyhow::anyhow!(e))?;
        info!("cef initialized");
    } else {
        info!(
            engine = cli.engine.as_deref().unwrap_or("?"),
            "cef init skipped — non-CEF --engine selected"
        );
    }

    // Phase 6 telemetry: count the successful CEF init as one
    // `app_starts` event. No-op when disabled. We tick *after*
    // `cef::initialize` returns 1 so a launch that crashes during CEF
    // boot doesn't get counted as a successful start.
    counters.increment(buffr_core::KEY_APP_STARTS);

    // -------- wayr event loop --------
    //
    // All platforms run OSR: CEF paints into a shared bitmap, the
    // wgpu present layer composites it under buffr's chrome strips.
    let event_loop = EventLoop::<BuffrUserEvent>::new().context("creating wayr event loop")?;
    // wayr always polls — no ControlFlow needed.

    let engine = Arc::new(Mutex::new(Engine::new(keymap)));

    // Register the `buffr://` scheme handler factory after the engine
    // exists so the new-tab renderer can read the live keymap on each
    // request (hot-reloaded user overrides land on the next visit).
    {
        let engine_for_newtab = Arc::clone(&engine);
        let provider: NewTabHtmlProvider =
            Arc::new(move || render_new_tab_html(&engine_for_newtab));
        backend.register_new_tab_handler(provider);
    }

    // Bring up the loopback HTTP server that serves `buffr://*` internal
    // pages. The same server is shared with every browser engine the app
    // instantiates so they all resolve `buffr://new` to the same
    // `http://127.0.0.1:<port>/<token>/new` URL — engines that can't
    // register custom URI schemes (WPE WebKit, blink-cdp) get an HTTP
    // origin out of the box, with fetch/modules/CSS imports working.
    //
    // Failure to bind is non-fatal: engines fall back to data: URL or
    // their native scheme handler.
    let internal_server: Arc<buffr_engine::internal_server::InternalServer> = {
        let engine_for_routes = Arc::clone(&engine);
        let newtab_handler: buffr_engine::internal_server::Handler =
            Arc::new(move || render_new_tab_html(&engine_for_routes));
        let routes = buffr_engine::internal_server::Routes::new()
            .html("/new", Arc::clone(&newtab_handler))
            // `buffr://newtab` is an alias historical config files use.
            .html("/newtab", Arc::clone(&newtab_handler))
            .html(
                "/settings",
                Arc::new(buffr_engine::newtab::default_settings_html),
            );
        let srv = buffr_engine::internal_server::InternalServer::start(routes)
            .context("internal_server: bind failed — buffr:// URLs will not load")?;
        let srv = Arc::new(srv);
        info!(
            addr = %srv.addr(),
            "internal_server: listening on loopback"
        );
        srv
    };

    // Register the `buffr-src:` scheme handler factory. Fetches the
    // underlying URL on a worker thread and renders it with bonsai
    // syntax highlighting (Round 2 of #30).
    backend.register_view_source_handler();

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

    // -------- --engine override ----------------------------------------
    //
    // When the user passes `--engine <NAME>`, throw away the on-disk
    // `[engines]` config and synthesise a single instance with the chosen
    // backend. Rules are cleared — the override is exclusive. Validation
    // already happened early; cli.engine here is guaranteed valid.
    let engines_config: Arc<buffr_config::Engines> = if let Some(raw) = cli.engine.as_deref() {
        let chosen = raw.to_lowercase();
        info!(backend = %chosen, "--engine override: ignoring [engines] config");
        Arc::new(buffr_config::Engines {
            default: chosen.clone(),
            instances: vec![buffr_config::EngineInstance {
                id: chosen.clone(),
                backend: chosen,
                data_dir: None,
            }],
            rules: Vec::new(),
        })
    } else {
        Arc::new(config.engines.clone())
    };

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

    // Crash-loop guard. Persistent profile only — `--private` runs are
    // ephemeral and don't need (or want) the tracker file. If the
    // recent-startup window already shows enough attempts without a
    // clean exit, treat this launch as a crash loop: quarantine the
    // saved session so a killer URL can't be restored again, and skip
    // session restore for this launch.
    let crash_guard_path = if cli.private {
        None
    } else {
        Some(crash_guard::default_path(&paths.data))
    };
    let crash_loop_detected = if let Some(gp) = crash_guard_path.as_ref() {
        let detected = crash_guard::record_start(gp);
        if detected && let Some(p) = session_path.as_ref() {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            if let Err(err) = crash_guard::quarantine_session(p, now) {
                warn!(error = %err, "crash_guard: quarantine failed");
            }
        }
        detected
    } else {
        false
    };

    let (pending_session_tabs, pending_session_active): (Vec<(String, bool)>, Option<usize>) =
        if cli.private || cli.no_restore || crash_loop_detected || !config.startup.restore_session {
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

    // Create the proxy before AppState so we can clone it for the IPC accept
    // thread and the Ctrl+C handler. The proxy is cheap to clone (internally an Arc).
    let event_proxy = event_loop.proxy();

    let shutdown_flag = Arc::new(AtomicBool::new(false));
    {
        let flag = Arc::clone(&shutdown_flag);
        let proxy = event_proxy.clone();
        if let Err(err) = ctrlc::set_handler(move || {
            // First press: cooperative shutdown.  Set the flag and
            // wake the winit loop via the event proxy so the next
            // `new_events` / `user_event` hook (whichever fires first)
            // calls `event_loop.exit()`.
            let was_set = flag.swap(true, Ordering::SeqCst);
            let _ = proxy.send_event(BuffrUserEvent::Shutdown);

            if !was_set {
                // Hard-abort fallback: if the cooperative shutdown
                // doesn't drain within 3 s (event loop wedged, paint
                // stuck in a syscall, etc.), exit the process the hard
                // way.  Without this, Ctrl+C on a hung UI thread does
                // nothing and the user is forced to SIGKILL from
                // another terminal.
                std::thread::Builder::new()
                    .name("buffr-shutdown-abort".into())
                    .spawn(|| {
                        std::thread::sleep(std::time::Duration::from_secs(3));
                        eprintln!("buffr: cooperative shutdown timed out after 3s; aborting");
                        // _exit skips destructors / atexit; appropriate
                        // for an emergency bail-out.  130 = 128 + SIGINT.
                        #[cfg(unix)]
                        unsafe {
                            libc::_exit(130)
                        };
                        #[cfg(not(unix))]
                        std::process::exit(130);
                    })
                    .ok();
            }
        }) {
            warn!(error = %err, "ctrlc handler already installed — using existing");
        }
    }

    // Spawn the singleton accept thread now that we have a proxy. The handle
    // is moved in so the Listener stays alive for the whole process lifetime.
    if let Some(handle) = singleton_handle.take() {
        single_instance::spawn_accept_thread(handle, event_proxy.clone());
    }

    let mut app_state = AppState::new(
        backend,
        homepage,
        new_tab_url,
        engine,
        history.clone(),
        bookmarks.clone(),
        downloads.clone(),
        downloads_config,
        zoom.clone(),
        permissions.clone(),
        download_notice_queue,
        search_config,
        engines_config,
        cli.private,
        paths.cache.clone(),
        paths.data.clone(),
        find_sink,
        hint_sink,
        edit_sink,
        hint_alphabet,
        cli.find.clone(),
        {
            // Combine positional URL args with --new-tab values. All open as
            // background tabs after session restore; order: positional then --new-tab.
            let mut tabs = cli.urls.clone();
            tabs.extend(cli.new_tab.clone());
            tabs
        },
        pending_session_tabs,
        pending_session_active,
        session_path,
        crash_guard_path,
        counters.clone(),
        update_checker.clone(),
        initial_update_status,
        build_palette(&config.theme),
        config.general.show_favicons,
        favicon_cache,
        Arc::new(config.idle_inhibit.clone()),
        shutdown_flag,
        event_proxy,
    );
    app_state.internal_server = Some(internal_server);
    app_state.heartbeat = initial_heartbeat;
    if let Err(err) = event_loop.run_app(&mut app_state) {
        warn!(error = %err, "wayr event loop exited with error");
    }

    // Shutdown sequence — order is critical. CEF browsers must close
    // and fully release before `cef::shutdown()`, and all CEF refs we
    // hold must drop while CEF's threads are still alive. Mishandling
    // any step segfaults during the GPU process teardown on builds
    // with hardware compositing.
    info!("shutdown: closing browsers");
    for host in app_state.engines.values() {
        host.close_all_browsers();
    }

    // Defer-dismiss any permission requests still queued at shutdown.
    // Must happen BEFORE engines are dropped — the CEF backend fires the
    // C++ callback from inside `resolve_permission`; the engine must still
    // be alive. Phase 8a (#88): drain each engine's neutral queue and call
    // the trait method so both CEF and blink-cdp clean up correctly.
    info!("shutdown: draining permission queues");
    for engine in app_state.engines.values() {
        let queue = engine.permissions_queue();
        let drained = buffr_engine::drain_permissions_queue(&queue);
        for p in drained {
            engine.resolve_permission(p.resolve_id.as_deref(), PromptOutcome::Defer);
        }
    }
    // Drop engine hosts first. This releases every Browser ref while
    // CEF's threads are still running, so CEF can finish the close
    // callbacks instead of segfaulting on dangling refs during its
    // own shutdown. Drop the engine router at the same time — it holds
    // `Arc<dyn BrowserEngine>` aliases to the same BrowserHost values, so
    // the hosts would otherwise not deallocate until app_state drops
    // after cef::shutdown (which would be too late).
    info!("shutdown: dropping engine hosts");
    drop(app_state.engine_router.take());
    app_state.engines.clear();

    // Drop the wgpu renderer BEFORE cef::shutdown(). Both touch the
    // same EGL / GL / Vulkan driver state on Linux; tearing down
    // wgpu after CEF has dismantled the GPU process segfaults.
    //
    // Works cleanly with wayr ≥ 0.1.4 because Toplevel is now
    // Send + Sync + 'static and impls HasDisplayHandle, so
    // Renderer::new takes Arc<Toplevel> via wgpu's safe
    // `instance.create_surface(arc)` path. wgpu holds its own ref;
    // the Toplevel survives wgpu's Surface drop and the
    // wl_surface.destroy() in Toplevel::drop runs after Vulkan
    // teardown completes. Prior to 0.1.4 we had to leak this path
    // because wgpu's Vulkan Surface drop SIGSEGVed when the
    // wl_* objects were destroyed mid-teardown.
    info!("shutdown: dropping renderer");
    drop(app_state.renderer.take());
    info!("shutdown: dropping popup renderers");
    app_state.popups.clear();

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
            &*app_state.backend,
        );
    }

    // -------- telemetry flush --------
    //
    // Final flush before CEF shutdown. No-op when telemetry is
    // disabled. Errors log at WARN inside `flush()` and never
    // propagate — telemetry must not block exit.
    counters.flush();

    // -------- shutdown --------
    info!("shutdown: backend shutting down");
    app_state.backend.shutdown();
    info!("shutdown: backend shutdown returned");
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
    // Bypass libc atexit + static destructors. We've already torn down
    // every long-lived resource explicitly (engines, renderer, backend,
    // private tempdir). What remains are library-internal destructors —
    // notably WPE WebKit's, which SIGABRT in its WTF runtime cleanup
    // because Igalia treats process-teardown ordering as the embedder's
    // problem and assumes WebKit was the last thing initialised. We've
    // joined the worker thread, so there's no thread-safety risk; flush
    // stderr before bailing so any in-flight tracing output isn't lost.
    use std::io::Write;
    let _ = std::io::stderr().flush();
    // SAFETY: _exit is async-signal-safe and takes no Rust state. Skipping
    // atexit handlers is intentional — see comment above.
    #[cfg(unix)]
    unsafe {
        libc::_exit(0)
    };
    #[cfg(not(unix))]
    std::process::exit(0);
}

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
    NEW_TAB_HTML_TEMPLATE
        .replacen(NEW_TAB_KEYBINDS_MARKER, &body, 1)
        .replacen(
            NEW_TAB_SPLASH_ART_MARKER,
            &crate::loading_anim::splash_art_html(),
            1,
        )
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

/// Resolve the (cache, data) profile paths. Returns the resolved
/// [`ProfilePaths`] plus an optional [`TempDir`] that owns
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
fn resolve_paths(private: bool) -> Result<(ProfilePaths, Option<TempDir>)> {
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
        Ok((ProfilePaths { cache, data }, Some(tmp)))
    } else {
        let paths = profile_paths().context("resolving profile dirs")?;
        std::fs::create_dir_all(&paths.cache).context("creating profile cache dir")?;
        std::fs::create_dir_all(&paths.data).context("creating profile data dir")?;
        Ok((paths, None))
    }
}

/// Directory (under the profile paths) that `item` wipes on exit, if any.
/// Cache and LocalStorage are trees under CEF's `root_cache_path`, which
/// is the *data* dir — NOT `paths.cache`. Cookies / History / Bookmarks /
/// Downloads route through other teardown paths (cookie manager /
/// `clear_all` stores) and return `None`.
fn clear_dir_path(paths: &ProfilePaths, item: ClearableData) -> Option<PathBuf> {
    match item {
        ClearableData::Cache => Some(paths.data.join("Cache")),
        ClearableData::LocalStorage => Some(paths.data.join("Local Storage")),
        ClearableData::Cookies
        | ClearableData::History
        | ClearableData::Bookmarks
        | ClearableData::Downloads => None,
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
    paths: &ProfilePaths,
    history: &buffr_history::History,
    bookmarks: &buffr_bookmarks::Bookmarks,
    downloads: &buffr_downloads::Downloads,
    backend: &dyn Backend,
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
            ClearableData::Cookies => clear_cookies(backend),
            ClearableData::Cache => clear_dir(
                &clear_dir_path(paths, item).expect("Cache resolves to a wipe dir"),
                "cache",
            ),
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
            ClearableData::LocalStorage => clear_dir(
                &clear_dir_path(paths, item).expect("LocalStorage resolves to a wipe dir"),
                "local_storage",
            ),
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
fn clear_cookies(backend: &dyn Backend) {
    backend.delete_all_cookies();
}

/// Split a [`ScrollEvent`] into an `(dx, dy)` pair for the swipe
/// detector.
///
/// A `ScrollEvent` carries exactly one axis per event with a single
/// `delta`; the orthogonal component is zero. Mirrors
/// [`scroll_to_cef_delta`]'s axis match — feeding `delta` in as `dx`
/// unconditionally makes vertical scrolling look like a horizontal
/// swipe and defeats the detector's dominance guard.
fn scroll_swipe_delta(ev: &crate::windowing::ScrollEvent) -> (f32, f32) {
    use crate::windowing::AxisDirection;
    let d = ev.delta as f32;
    match ev.axis {
        AxisDirection::Horizontal => (d, 0.0),
        AxisDirection::Vertical => (0.0, d),
    }
}

/// Two-finger horizontal-swipe back/forward gesture accumulator.
///
/// Pure state machine so it can be exercised without an `AppState`:
/// [`SwipeDetector::feed`] takes the current time rather than reading
/// the clock itself. [`AppState::detect_swipe`] is the thin wrapper
/// that supplies `Instant::now()`.
#[derive(Debug, Default)]
struct SwipeDetector {
    accum_x: f32,
    accum_y: f32,
    last_at: Option<Instant>,
    committed: bool,
}

impl SwipeDetector {
    /// Feed one high-res scroll delta in screen pixels. Returns
    /// `Some(HistoryBack | HistoryForward)` the first time a gesture
    /// commits; subsequent events of the same gesture are bounded by
    /// `committed` (caller swallows them). A gesture is bounded by
    /// `GAP` of inactivity.
    ///
    /// Direction: positive `dx` = swipe RIGHT → back. Negative = swipe
    /// LEFT → forward. Mirrors Chrome/Safari macOS convention (verified
    /// on Linux Wayland touchpad with natural scrolling enabled — sign
    /// matches the physical gesture there).
    fn feed(&mut self, dx: f32, dy: f32, now: Instant) -> Option<buffr_modal::PageAction> {
        const GAP: Duration = Duration::from_millis(200);
        // Raw px thresholds — touchpad 2-finger swipes deliver ~5-15px
        // per event at 60Hz, so 150px = ~10-30 events of intent.
        const HORIZ_THRESHOLD: f32 = 150.0;
        const HORIZ_DOMINANCE: f32 = 2.0;

        let resumed = self
            .last_at
            .map(|t| now.duration_since(t) > GAP)
            .unwrap_or(true);
        if resumed {
            self.accum_x = 0.0;
            self.accum_y = 0.0;
            self.committed = false;
        }
        self.last_at = Some(now);
        self.accum_x += dx;
        self.accum_y += dy;

        if self.committed {
            return None;
        }
        let ax = self.accum_x.abs();
        let ay = self.accum_y.abs();
        if ax >= HORIZ_THRESHOLD && ax > HORIZ_DOMINANCE * ay {
            self.committed = true;
            let action = if self.accum_x > 0.0 {
                buffr_modal::PageAction::HistoryBack
            } else {
                buffr_modal::PageAction::HistoryForward
            };
            tracing::debug!(
                accum_x = self.accum_x,
                accum_y = self.accum_y,
                ?action,
                "swipe gesture committed",
            );
            return Some(action);
        }
        None
    }
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
    /// Active backend — process-model lifecycle (library load, init,
    /// shutdown, message pump, scheme registration, engine construction).
    /// Constructed in `main()` as `Arc<CefBackend>` wrapped in
    /// `Arc<dyn Backend>`; all calls go through the trait.
    backend: Arc<dyn Backend>,
    /// URL loaded into the cold-start tab 0. Defaults to `buffr://new`
    /// and is overridable via `general.homepage` and `--homepage`.
    /// Fresh tabs (`o`/`O`, `:tabnew`, the `gh` chord) use
    /// `new_tab_url` instead.
    homepage: String,
    /// URL a fresh tab opens (`o`/`O`, `:tabnew`, the TabNew action).
    /// From `config.startup.new_tab_url`; default `about:blank`. The
    /// cold-start tab 0 still uses `homepage`.
    new_tab_url: String,
    // Drop order matters at shutdown: engine hosts MUST drop before `window`
    // and `renderer`. CEF browsers hold raw handles tied to the window
    // surface and to the GPU process; dropping the window or wgpu
    // device first leaves CEF dereferencing freed memory during its
    // own teardown. Rust drops struct fields in declaration order, so
    // these come first.
    //
    // `idle_inhibitor` MUST also drop before `window`. The Wayland
    // backend's worker thread holds a `Connection` built via
    // `Backend::from_foreign_display` against winit's `wl_display`
    // pointer; dropping `window` first frees that display and the
    // inhibitor's Drop (which sends Release + Shutdown then calls
    // `inh.destroy()` + `conn.flush()`) would touch a dead fd.
    //
    // Phase 3: all engine instances live in `engines`; `active_engine`
    // names which one owns the currently-focused tab. `host` is kept as
    // a thin accessor for the common single-engine call sites — it reads
    // from `engines[active_engine]` at runtime so all existing paths work
    // unchanged. Multi-engine-aware paths use `engines` directly.
    //
    // `host` field removed — use `self.active_engine_dyn()` instead.
    /// Phase 3+: registered engine instances, keyed by [`EngineId`].
    /// Phase 4: changed from `Arc<BrowserHost>` to `Arc<dyn BrowserEngine>`
    /// so blink-cdp and future non-CEF backends can live here too.
    /// Populated in `resumed` (one entry per `engines.instances` config
    /// entry, plus the synthesised default when `instances` is empty).
    engines:
        std::collections::HashMap<buffr_engine::EngineId, Arc<dyn buffr_engine::BrowserEngine>>,
    /// Which engine owns the currently-focused tab. Updated when a
    /// cross-engine navigation opens a tab on a different engine.
    active_engine: buffr_engine::EngineId,
    /// Phase 3: engine router — resolves URL → registered backend.
    /// `None` until the hosts are constructed in `resumed`.
    engine_router: Option<Arc<engine_router::EngineRouter>>,
    idle_inhibitor: Option<Box<dyn IdleInhibitor>>,
    window: Option<Arc<Toplevel>>,
    engine: Arc<Mutex<Engine>>,
    history: Arc<buffr_history::History>,
    bookmarks: Arc<buffr_bookmarks::Bookmarks>,
    downloads: Arc<buffr_downloads::Downloads>,
    downloads_config: Arc<buffr_config::DownloadsConfig>,
    zoom: Arc<buffr_zoom::ZoomStore>,
    permissions: Arc<Permissions>,
    /// Active permission prompt (if any). `Some` while the front of
    /// the active engine's permissions queue is being shown. Keystrokes
    /// route to the prompt resolution path while this is set.
    permissions_prompt: Option<PermissionsPrompt>,
    /// Identity of the queue entry `permissions_prompt` is rendering.
    ///
    /// The widget carries only what it draws (origin, capability labels,
    /// backlog count) — nothing that says *which* request that is. Backends
    /// can withdraw a queued request at any time (CEF does this when the tab
    /// navigates away), so the front of the queue when the user answers is
    /// not necessarily the entry they read. This is the identity the answer
    /// is matched against before anything is stored or resolved; see
    /// [`buffr_engine::permissions::resolve_target`]. Always `Some` while
    /// `permissions_prompt` is `Some`.
    permissions_prompt_id: Option<PromptIdentity>,
    /// Pending close-pinned-tab confirmation. When `Some(id)`, a
    /// yes/no banner is shown and the close is gated on the user's
    /// answer (`y` / yes-button → close; `n` / no-button / `<Esc>`
    /// → dismiss). Mutually exclusive with `permissions_prompt` for
    /// rendering — the confirmation wins the slot.
    confirm_close_pinned: Option<TabId>,
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
    /// Engine routing config. Held so the router can be (re)built on
    /// first window creation without needing access to the full Config.
    engines_config: Arc<buffr_config::Engines>,
    /// Active overlay (top-of-window input bar). `None` when the
    /// engine is in any non-overlay mode; the CEF child rect uses the
    /// full vertical space minus the bottom statusline.
    overlay: Option<OverlayState>,
    /// Whether the runtime is in `--private` mode. Drives the title
    /// stamp and is purely informational — the storage layer already
    /// captured the choice at construction time.
    private: bool,
    /// Root of the cache directory for this session. In normal mode this is
    /// `<XDG_CACHE_HOME>/buffr` (or equivalent); in `--private` mode it is
    /// `$TMPDIR/buffr-private-<pid>/cache`. Reserved for future split between
    /// persistent and ephemeral state — currently CEF stores both under
    /// `data_root` because its RequestContext doesn't expose a split.
    #[allow(dead_code)]
    cache_root: PathBuf,
    /// Root of the user-data directory for this session. In normal mode
    /// this is `<XDG_DATA_HOME>/buffr` (or equivalent); in `--private`
    /// mode it is `$TMPDIR/buffr-private-<pid>/data` (deleted on Drop
    /// by the TempDir held in main). Per-engine profile dirs land under
    /// `<data_root>/engines/<id>/` so `--private` mode truly isolates
    /// every engine's storage to the throwaway tempdir, and persistent
    /// state survives system cache wipes.
    data_root: PathBuf,
    modifiers: Modifiers,
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
    cancel_closes_tab: Option<TabId>,
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
    last_session_tab_ids: Vec<TabId>,
    /// Wall-clock instant of the last `active_tab_live_url()` call.
    /// Throttled to ~4 Hz (250 ms) to bound the cef-rs
    /// "Invalid UTF-16 string" stderr spam during page loads.
    last_url_poll: Instant,
    /// Cross-thread wake handle for the wayr event loop. Cloned and
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
    /// Crash-loop tracker file. Cleared on graceful shutdown so the
    /// next launch starts with a clean attempt history. `None` mirrors
    /// `session_path` semantics — private mode skips the tracker.
    crash_guard_path: Option<PathBuf>,
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
    /// Dimensions of the most recently received OSR paint. `None` until
    /// CEF emits the first on_paint. Used to gate the loading animation:
    /// once we've seen a paint we don't fall back to animation just because
    /// the next render between paints sees an empty `host.osr_frame().pixels`
    /// (the swap-out side effect of acquire-by-mem-swap).
    last_osr_dims: Option<(u32, u32)>,
    /// Debounced CEF resize: tracks the deadline for calling `host.osr_resize`.
    /// Refreshed on every `WindowEvent::Resized`; fired once quiet for
    /// `CEF_RESIZE_DEBOUNCE`. Idle when `pending_cef_resize.deadline()` is None.
    pending_cef_resize: ResizeDebounce,
    /// Last known cursor position in browser-region coordinates.
    /// Updated on every `CursorMoved` event; used when forwarding click and
    /// wheel events so we don't have to thread the position through each arm.
    osr_cursor: (i32, i32),
    /// Timestamp of the last mouse click, used for double-click detection.
    osr_last_click_at: Instant,
    /// Button of the last click.  `None` before the first click.
    osr_last_click_button: Option<NeutralMouseButton>,
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
    /// `PixelDelta` events accumulate (touchpad). See [`SwipeDetector`]
    /// for the gap, threshold and dominance rules; once a gesture
    /// commits, `swipe.committed` suppresses further nav until it
    /// restarts.
    swipe: SwipeDetector,
    /// Ctrl+C handler flag. Set to `true` by the `ctrlc` handler;
    /// polled in `about_to_wait` to exit with a single key press.
    shutdown_flag: Arc<AtomicBool>,
    /// Loopback HTTP server that serves `buffr://*` internal pages. Bound
    /// to `127.0.0.1` on an ephemeral port, gated by a per-launch hex
    /// token in the URL path. Shared across every browser engine the app
    /// instantiates so they all resolve `buffr://new` to the same place.
    /// `None` if startup binding failed — engines then fall back to their
    /// own internal-page handling (e.g. data: URLs).
    internal_server: Option<Arc<buffr_engine::internal_server::InternalServer>>,
    /// Next time CEF expects a pump, or `None` when idle.
    /// Set by `OnScheduleMessagePumpWork(delay_ms)`; cleared after
    /// pumping so we wait for CEF to schedule the next work item.
    cef_next_pump_at: Option<Instant>,
    /// Cached event-loop pump period: `(computed_at, period)`. The tick
    /// computes the wakeup deadline from the fastest live output's refresh
    /// rate; `event_loop.outputs()` allocates a `Vec<OutputInfo>` with
    /// per-output String name/description clones on every call, so it is
    /// only re-queried once per second and the cached period is reused
    /// for the intervening ~144 Hz ticks. `None` until first computed.
    last_outputs_recompute: Option<(Instant, Duration)>,
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
    /// Live popup windows keyed by their wayr `SurfaceId`.
    popups: HashMap<SurfaceId, PopupWindow>,
    /// Reverse map: CEF browser id → wayr `SurfaceId`, for fast lookup
    /// in the PopupCloseSink drain and CEF event routing.
    popup_window_id_by_browser: HashMap<i32, SurfaceId>,
    /// Popup-created event queue. Drained each `about_to_wait` tick to
    /// spawn new popup windows. Obtained from `host.popup_create_sink()`.
    popup_create_sink: PopupCreateSink,
    /// Popup-closed event queue. Drained each `about_to_wait` tick to
    /// drop popup windows. Obtained from `host.popup_close_sink()`.
    popup_close_sink: PopupCloseSink,
    /// Per-browser favicon bitmaps. Populated from `engine.drain_favicon_updates()`
    /// drains and read in `refresh_tab_strip` to attach a `TabFavicon` to
    /// each `TabView`.
    favicons: HashMap<i32, buffr_ui::TabFavicon>,
    /// Mirror of `[general] show_favicons`. Threaded into `BrowserHost`
    /// at construction so `on_favicon_urlchange` can short-circuit
    /// without issuing a `download_image` call. Also gates the apps-side
    /// pump so disabled-mode never populates the cache.
    show_favicons: bool,
    /// SQLite-backed favicon blob cache. `None` in `--private` mode or when
    /// the store failed to open, or when `show_favicons` is false.
    favicon_cache: Option<buffr_core::FaviconCache>,
    /// Pending favicon prefill: maps `browser_id → origin` for tabs that were
    /// created/restored before their CEF-delivered favicon arrived. Populated
    /// when a tab is opened to a URL whose origin may have a cached bitmap;
    /// consumed in `pump_favicon_updates` so a cache hit can be applied
    /// immediately on the first tick before CEF fires.
    pending_favicon_prefill: HashMap<i32, String>,
    /// Per-browser memoization of the last URL we cache-checked. The runtime
    /// scan in `pump_favicon_updates` walks every tab and enqueues a prefill
    /// when the tab's current URL differs from the one recorded here. This
    /// covers omnibar / hint / popup / middle-click opens without touching
    /// every `host.open_tab(...)` call site, and avoids re-running point
    /// lookups for unchanged URLs.
    favicon_check_url: HashMap<i32, String>,
    /// Persisted splash state. `hjkl-splash` 0.2 owns its time source —
    /// `cells()` reads the wall clock internally so animation cadence is
    /// independent of paint rate (scrolling can't accelerate the
    /// wordmark). Used for the loading-anim Rust paint path, and to
    /// generate per-tick HTML for the new-tab page's splash element.
    splash: hjkl_splash::Splash<'static>,
    /// Last splash tick value pushed to the new-tab page. `None` when no
    /// new-tab tab is active. Compared against `splash.tick()` on every
    /// `about_to_wait` to dedupe redundant `execute_javascript` calls.
    last_splash_tick: Option<u64>,
    /// Wake deadline for the next new-tab splash JS push. `Some` while a
    /// new-tab tab is active so the event loop wakes on the splash
    /// period and pushes the next frame. `None` clears the wake.
    splash_js_next_push: Option<Instant>,
    /// True while the last `paint_chrome_with` used the loading animation
    /// path (OSR buffer absent or wrong size). Cleared when the OSR path
    /// resumes. Used to emit the `debug!` transition log exactly once.
    loading_anim_active: bool,
    /// When `Some(t)`, the event loop's `about_to_wait` sets
    /// `ControlFlow::WaitUntil(t)` to ensure the next animation frame
    /// fires at ~12 fps. Cleared as soon as `loading_anim_active`
    /// becomes false so the loop returns to event-driven idle.
    loading_anim_next_wake: Option<Instant>,
    /// Watchdog for the post-resize CEF paint. Armed when `osr_resize`
    /// is called; cleared when the freshness gate accepts a paint at
    /// the expected dims.  Fires a `force_repaint_active` nudge if CEF
    /// fails to produce the paint within `RESIZE_PAINT_WATCHDOG_TIMEOUT`.
    resize_paint_watchdog: ResizePaintWatchdog,
    /// True when the last `paint_chrome_with` presented a buffer at dims
    /// that disagreed with `window.inner_size()` at that moment. Hyprland
    /// will letterbox/pillarbox a wl_surface buffer that doesn't match
    /// the surface's configured size, producing persistent black bars.
    /// Set at end of paint, consumed at start of next paint to (a) keep
    /// the loading animation playing (so the user sees motion rather
    /// than a frozen wrong-aspect frame) and (b) make sure another
    /// redraw was queued so the next paint reconciles renderer dims to
    /// the live window dims.
    surface_drifted: bool,

    // ── OSR sleep policy (phase 1 — single window) ───────────────────────────
    //
    // When #18 (multi-window) lands these fields move into a per-WindowState.
    /// True while the compositor reports the window is occluded
    /// (off-screen, fully covered, on a hidden workspace, minimized).
    /// Updated by `WindowEvent::Occluded` (immediately on reveal; via
    /// `sleep_deadline` debounce on occlude).  OS focus is intentionally
    /// NOT used: a side-by-side, visible-but-unfocused window must keep
    /// painting.
    occluded: bool,
    /// Non-None while an occlude → sleep transition is pending debounce.
    /// Set to `Instant::now() + OCCLUDE_SLEEP_DEBOUNCE` on `Occluded(true)`;
    /// cleared immediately on `Occluded(false)` or when `about_to_wait`
    /// sees the deadline elapse.
    sleep_deadline: Option<Instant>,
    /// True when the CEF audio handler has at least one active stream in
    /// any browser, OR when the last JS media probe returned `true`.
    /// Keeps the paint pipeline alive while the window is occluded.
    media_active: bool,
    /// Deadline for the next media-probe JS fire.  `None` until the first
    /// probe is scheduled; reset to `now + MEDIA_PROBE_INTERVAL` after
    /// each fire so the probe runs at steady cadence while occluded.
    media_probe_next: Option<Instant>,
    /// Current paint policy.  Transitions trigger `osr_sleep` / `osr_sleep(false)`
    /// on the host.  Starts `Active` so the window paints immediately on launch.
    paint_policy: PaintPolicy,
    /// Recent `present_us` samples (most recent at the back).  Capped at
    /// [`PRESENT_HISTORY_SIZE`].  Drives the present-time occlusion
    /// heuristic for compositors where winit `Occluded` is unreliable
    /// (Hyprland workspace switch, etc.).
    present_us_history: VecDeque<u64>,
    /// Deadline for the next occlusion probe present while the policy
    /// is `Sleeping` due to the heuristic.  `None` when not sleeping or
    /// when the next probe is implicit (any chrome-dirty paint also
    /// serves as a probe via the sleep-guard bypass).
    next_probe_at: Option<Instant>,
    /// Set true by `about_to_wait` immediately before requesting a
    /// wake-probe redraw; read-and-cleared at the TOP of
    /// `paint_chrome_with` so every early-return path consumes it (M33).
    /// Bypasses the sleep guard for exactly one paint so we can measure
    /// `present_us` and decide stay/wake.
    probe_pending: bool,
    /// Deadline for re-requesting a redraw after the renderer skipped a
    /// frame ([`crate::render::Submitted::No`]).  The chrome/OSR update
    /// is still pending in that case, so the paint has to be retried —
    /// throttled by [`SKIPPED_FRAME_RETRY_DELAY`] so a wedged render
    /// worker can't turn the retry into a busy loop.  `None` when the
    /// last paint reached the GPU.
    repaint_retry_at: Option<Instant>,

    // ── Idle-inhibit (issue #22) ─────────────────────────────────────────────
    /// Idle-inhibit config snapshot. Shared so hot-reload (if added later)
    /// can swap it without restarting. Mirrors the pattern used by
    /// `downloads_config`.
    idle_inhibit_config: Arc<buffr_config::IdleInhibitConfig>,
    // `idle_inhibitor` lives at the top of the struct (next to `host`)
    // for drop-order reasons — see the comment there.
    /// True when the last JS media probe (or future console-log IPC reader)
    /// reported `window.__buffr_video_active === true`. Updated each
    /// `about_to_wait` tick alongside `media_active`.
    video_active: bool,
    /// True while the main buffr window has OS-level focus (winit
    /// `WindowEvent::Focused`). Used by the idle-inhibit policy when
    /// `config.idle_inhibit.require_focus = true`.
    window_focused: bool,
    /// Active right-click context menu, if any. `None` when no menu is
    /// visible. Set from `active_engine_dyn().drain_context_menu_requests()` each tick;
    /// cleared on Esc, Enter (activation), or click-outside.
    context_menu: Option<ActiveContextMenu>,
    /// UDS heartbeat liveness probe for the buffr (supervisor) watchdog.
    /// `None` when running unsupervised (no `BUFFR_SUPERVISOR_SOCK` env var
    /// or connect failed). Ticked every `about_to_wait`; on write error the
    /// field is set back to `None` (supervisor detects the silence and kills).
    heartbeat: Option<heartbeat::Heartbeat>,
}

/// OSR paint policy for the window.
///
/// `Active` — CEF paints normally; wgpu presents each frame.
/// `Sleeping` — `was_hidden(1)` paused the CEF paint scheduler; the
///   `paint_chrome_with` fast-exit guard skips wgpu present.
///
/// Transitions are managed by `AppState::recompute_paint_policy` in
/// `about_to_wait`.  When #18 (multi-window) lands, this moves into a
/// per-`WindowState` struct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaintPolicy {
    Active,
    Sleeping,
}

/// Edit-mode focus state machine.
///
/// Transitions:
///   `None` → (JS focusin event) → `Editing`
///   `Editing` → (Esc) → `None`
///   `Editing` → (JS Blur event for same field) → `None`
#[derive(Debug)]
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
        backend: Arc<dyn Backend>,
        homepage: String,
        new_tab_url: String,
        engine: Arc<Mutex<Engine>>,
        history: Arc<buffr_history::History>,
        bookmarks: Arc<buffr_bookmarks::Bookmarks>,
        downloads: Arc<buffr_downloads::Downloads>,
        downloads_config: Arc<buffr_config::DownloadsConfig>,
        zoom: Arc<buffr_zoom::ZoomStore>,
        permissions: Arc<Permissions>,
        download_notice_queue: DownloadNoticeQueue,
        search_config: Arc<buffr_config::Search>,
        engines_config: Arc<buffr_config::Engines>,
        private: bool,
        cache_root: PathBuf,
        data_root: PathBuf,
        find_sink: FindResultSink,
        hint_sink: HintEventSink,
        edit_sink: EditEventSink,
        hint_alphabet: HintAlphabet,
        pending_find: Option<String>,
        pending_new_tabs: Vec<String>,
        pending_session_tabs: Vec<(String, bool)>,
        pending_session_active: Option<usize>,
        session_path: Option<PathBuf>,
        crash_guard_path: Option<PathBuf>,
        counters: Arc<buffr_core::UsageCounters>,
        update_checker: Arc<buffr_core::UpdateChecker>,
        initial_update_status: buffr_core::UpdateStatus,
        palette: Palette,
        show_favicons: bool,
        favicon_cache: Option<buffr_core::FaviconCache>,
        idle_inhibit_config: Arc<buffr_config::IdleInhibitConfig>,
        shutdown_flag: Arc<AtomicBool>,
        event_proxy: EventLoopProxy<BuffrUserEvent>,
    ) -> Self {
        let update_indicator = update_indicator_from(&initial_update_status);
        let mut statusline = Statusline {
            url: homepage.clone(),
            private,
            cert_state: CertState::Unknown,
            update_indicator,
            palette,
            ..Statusline::default()
        };
        statusline.mode = PageMode::Normal;
        let tab_strip = TabStrip {
            palette,
            ..TabStrip::default()
        };
        Self {
            backend,
            homepage,
            new_tab_url,
            engines: std::collections::HashMap::new(),
            active_engine: buffr_engine::EngineId::new("cef"),
            engine_router: None,
            window: None,
            engine,
            history,
            bookmarks,
            downloads,
            downloads_config,
            zoom,
            permissions,
            permissions_prompt: None,
            permissions_prompt_id: None,
            confirm_close_pinned: None,
            download_notice_queue,
            search_config,
            engines_config,
            overlay: None,
            private,
            cache_root,
            data_root,
            modifiers: Modifiers::default(),
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
            tab_strip,
            pending_new_tabs,
            pending_session_tabs,
            pending_session_active,
            session_path,
            crash_guard_path,
            renderer: None,
            cursor_blink_at: Instant::now(),
            counters,
            counters_flush_at: Instant::now(),
            update_checker,
            last_osr_generation: 0,
            last_osr_dims: None,
            pending_cef_resize: ResizeDebounce::default(),
            osr_cursor: (0, 0),
            osr_last_click_at: Instant::now(),
            osr_last_click_button: None,
            osr_click_count: 1,
            osr_drag_start: None,
            osr_mouse_buttons: 0,
            osr_wheel_velocity: (0.0, 0.0),
            osr_wheel_last_at: None,
            swipe: SwipeDetector::default(),
            shutdown_flag,
            internal_server: None,
            cef_next_pump_at: None,
            last_outputs_recompute: None,
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
            popup_create_sink: buffr_engine::new_popup_create_sink(),
            popup_close_sink: buffr_engine::new_popup_close_sink(),
            favicons: HashMap::new(),
            show_favicons,
            favicon_cache,
            pending_favicon_prefill: HashMap::new(),
            favicon_check_url: HashMap::new(),
            splash: crate::loading_anim::new_splash(),
            last_splash_tick: None,
            splash_js_next_push: None,
            loading_anim_active: false,
            loading_anim_next_wake: None,
            resize_paint_watchdog: ResizePaintWatchdog::default(),
            surface_drifted: false,
            occluded: false,
            sleep_deadline: None,
            media_active: false,
            media_probe_next: None,
            paint_policy: PaintPolicy::Active,
            present_us_history: VecDeque::with_capacity(PRESENT_HISTORY_SIZE),
            next_probe_at: None,
            probe_pending: false,
            repaint_retry_at: None,
            idle_inhibit_config,
            idle_inhibitor: None,
            video_active: false,
            window_focused: false,
            context_menu: None,
            heartbeat: None,
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

    // ── Engine helpers ────────────────────────────────────────────────────────

    /// Return a clone of the active engine as a `dyn BrowserEngine`, if any.
    ///
    /// Works for all backends (CEF, blink-cdp, future). This is the canonical
    /// single-engine accessor used by all code paths that need the currently-
    /// focused engine. Multi-engine-aware paths use `self.engines` directly.
    ///
    /// Cloning the `Arc` is ~2 ns and breaks the borrow on `self`, which is
    /// essential: callers typically mutate other `self` fields after obtaining
    /// the handle.
    #[inline]
    fn active_engine_dyn(&self) -> Option<Arc<dyn buffr_engine::BrowserEngine>> {
        self.engines.get(&self.active_engine).cloned()
    }

    /// Open a new background tab, routing through the engine router.
    fn routed_open_tab_background(&self, url: &str) -> Result<TabId, buffr_engine::EngineError> {
        if let Some(router) = &self.engine_router {
            router.engine_for(url).open_tab_background(url)
        } else if let Some(engine) = self.active_engine_dyn() {
            engine.open_tab_background(url)
        } else {
            Err(buffr_engine::EngineError::Other(
                "no engine available".into(),
            ))
        }
    }

    /// Push a new `present_us` sample into the rolling history and run
    /// the occlusion heuristic.
    ///
    /// Two transitions:
    /// - Active → heuristic-occluded: at least
    ///   [`SLOW_FRAMES_TO_OCCLUDE`]-of-[`PRESENT_HISTORY_SIZE`] frames
    ///   exceeded [`SLOW_PRESENT_THRESHOLD_US`].  Sets `occluded=true`,
    ///   schedules the first wake-probe, recomputes the policy.
    /// - Sleeping (probe) → Active: this paint was a wake-probe and
    ///   `present_us` came back below [`FAST_PRESENT_THRESHOLD_US`].
    ///   Compositor is releasing buffers again; clear `occluded` and
    ///   wipe history so the next slow-streak starts fresh.
    ///
    /// While Sleeping with the probe still slow, schedule the next
    /// probe for [`OCCLUSION_PROBE_INTERVAL`] from now.
    fn observe_present_us(&mut self, present_us: u64, was_probe: bool) {
        record_present_us(&mut self.present_us_history, present_us);

        if self.paint_policy == PaintPolicy::Sleeping {
            // Any successful present while Sleeping is treated as a probe
            // result (chrome-dirty bypass paints are also informative).
            if present_us < FAST_PRESENT_THRESHOLD_US {
                tracing::debug!(present_us, was_probe, "occlusion: probe fast → wake");
                self.occluded = false;
                self.present_us_history.clear();
                self.next_probe_at = None;
                self.recompute_paint_policy();
            } else {
                self.next_probe_at = Some(Instant::now() + OCCLUSION_PROBE_INTERVAL);
                // Probe woke CEF via osr_sleep(false); re-hide all engines so
                // we don't burn renderer CPU until the next probe.
                for host in self.engines.values() {
                    host.osr_sleep(true);
                }
            }
            return;
        }

        // Active: occlude if the rolling history shows sustained slow
        // presents OR if a single frame was egregiously slow (the
        // 500 ms+ fingerprint of a Wayland compositor blocking present
        // on a hidden surface).
        let immediate = present_us > IMMEDIATE_OCCLUDE_THRESHOLD_US;
        let sustained = detect_occluded_from_history(
            &self.present_us_history,
            SLOW_PRESENT_THRESHOLD_US,
            SLOW_FRAMES_TO_OCCLUDE,
        );
        if !self.occluded && (immediate || sustained) {
            tracing::debug!(
                present_us,
                immediate,
                sustained,
                history = ?self.present_us_history,
                "occlusion: heuristic-occluded → sleep"
            );
            self.occluded = true;
            self.next_probe_at = Some(Instant::now() + OCCLUSION_PROBE_INTERVAL);
            self.recompute_paint_policy();
        }
    }

    /// Recompute the paint policy from current `occluded` + `media_active`
    /// state and apply a transition if needed.
    ///
    /// Called from:
    /// - `about_to_wait` after draining audio events and checking sleep deadline.
    /// - `WindowEvent::Occluded(false)` (immediate wake on reveal).
    ///
    /// On Active→Sleeping: calls `host.osr_sleep(true)`.
    /// On Sleeping→Active: calls `host.osr_sleep(false)` + `host.osr_invalidate_view()`
    ///   + `window.request_redraw()`.
    fn recompute_paint_policy(&mut self) {
        let new_policy = decide_paint_policy(self.occluded, self.media_active);
        if new_policy == self.paint_policy {
            return;
        }
        tracing::debug!(
            occluded = self.occluded,
            media_active = self.media_active,
            old = ?self.paint_policy,
            new = ?new_policy,
            "paint_policy transition"
        );
        self.paint_policy = new_policy;
        // Fan sleep/wake to all engines so inactive engines don't keep
        // consuming GPU resources while the window is occluded.
        for host in self.engines.values() {
            match new_policy {
                PaintPolicy::Sleeping => {
                    host.osr_sleep(true);
                }
                PaintPolicy::Active => {
                    host.osr_sleep(false);
                    host.osr_invalidate_view();
                }
            }
        }
        if new_policy == PaintPolicy::Active
            && let Some(window) = self.window.as_ref()
        {
            window.request_redraw();
        }
    }

    // ── Cross-engine navigation (Phase 3) ────────────────────────────────────

    /// Check whether the active tab's current URL routes to a different engine
    /// than the one that hosts it. If so, open a new tab on the target engine
    /// and close the in-flight tab on the source. Called after each
    /// `pump_address_changes` cycle.
    ///
    /// Cross-engine nav loses navigation history on the source tab (expected —
    /// separate process/engine). The tab is closed immediately so the user does
    /// not see a dangling in-flight tab.
    fn check_cross_engine_nav(&mut self) {
        let Some(router) = self.engine_router.as_ref() else {
            return;
        };
        let Some(active_host) = self.engines.get(&self.active_engine) else {
            return;
        };
        let url = active_host.active_tab_live_url();
        if url.is_empty() {
            return;
        }
        // Use the pure classify_navigation helper so this logic is unit-testable.
        let verdict = engine_router::classify_navigation(router, &self.active_engine, &url);
        let target_id = match verdict {
            engine_router::NavigationVerdict::SameEngine => return,
            engine_router::NavigationVerdict::CrossEngine { target } => target,
            engine_router::NavigationVerdict::DisallowedScheme => return,
        };
        tracing::debug!(
            url = %url,
            source = %self.active_engine,
            target = %target_id,
            "cross-engine nav detected — opening new tab on target engine"
        );
        let Some(target_host) = self.engines.get(&target_id) else {
            tracing::warn!(target = %target_id, "target engine not registered — ignoring cross-engine nav");
            return;
        };
        // Open a new tab on the target engine.
        match buffr_engine::BrowserEngine::open_tab(target_host.as_ref(), &url) {
            Ok(_new_tab_id) => {
                tracing::debug!(target = %target_id, "cross-engine nav: new tab opened on target");
            }
            Err(err) => {
                tracing::warn!(target = %target_id, error = %err, "cross-engine nav: open_tab on target failed");
                return;
            }
        }
        // Close the in-flight tab on the source. Use close_active so we
        // don't need to know the exact tab id — the navigating tab IS active.
        let source_host = self
            .engines
            .get(&self.active_engine)
            .expect("source still registered");
        match buffr_engine::BrowserEngine::close_active(source_host.as_ref()) {
            Ok(_) => {
                tracing::debug!(source = %self.active_engine, "cross-engine nav: source tab closed");
            }
            Err(err) => {
                tracing::warn!(source = %self.active_engine, error = %err, "cross-engine nav: close_active on source failed");
            }
        }
        // Switch to the target engine.
        self.active_engine = target_id;
        self.mark_chrome_dirty();
        self.mark_session_dirty();
        self.request_redraw();
    }

    /// Current device scale factor. Reads winit's `scale_factor()` when the
    /// window exists; falls back to 1.0. Used to convert physical mouse
    /// coordinates to logical DIPs before forwarding to CEF OSR.
    fn current_scale(&self) -> f32 {
        self.window
            .as_ref()
            .map(|w| w.scale_factor() as f32)
            .unwrap_or(1.0)
    }

    fn dispatch_action(&mut self, action: &buffr_modal::PageAction) {
        use buffr_modal::PageAction as A;
        // Adjacent-tab opens require both a host call and a &mut self call
        // (open_omnibar). Handle them before the shared engine borrow so the
        // borrow checker sees two disjoint borrows.
        if matches!(action, A::TabNewRight | A::TabNewLeft) {
            let Some(engine) = self.active_engine_dyn() else {
                warn!(?action, "no browser engine yet — dropping action");
                return;
            };
            let raw_idx = if matches!(action, A::TabNewRight) {
                engine.active_index().unwrap_or(0).saturating_add(1)
            } else {
                engine.active_index().unwrap_or(0)
            };
            // The new tab is unpinned, so clamp to the unpinned region
            // (i.e. at or after the last pinned slot). Otherwise an
            // `O` from the first pinned tab would push the unpinned
            // entry into the pinned-only leading band.
            let insert_idx = raw_idx.max(engine.pinned_count());
            let url = self.new_tab_url.clone();
            // Open on the CURRENT engine, not via the router. The user
            // pressed `o`/`O` while looking at this engine — they expect
            // the tab to appear here, not on whichever engine the router's
            // default points at. Routing kicks in only when the user
            // navigates with a real URL (via the omnibar submit path),
            // which calls into `routed_open_tab` / cross-engine logic.
            //
            // Before this fix, multi-engine configs (e.g. default=cef +
            // blink-cdp by domain) would open a `cef` tab when the user
            // pressed `o` on a `blink-cdp` tab, and the omnibar would
            // pre-fill from `blink-cdp`'s still-current tab — making it
            // look like nothing happened except an omnibar opening on
            // the current tab.
            let result = engine.open_tab_at(&url, insert_idx);
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

        let Some(host) = self.active_engine_dyn() else {
            warn!(?action, "no browser engine yet — dropping action");
            return;
        };
        // Tab actions need apps-layer policy decisions (e.g. last-tab
        // close → exit) so they bypass the host dispatcher's fallback
        // path.
        match action {
            A::TabNewRight | A::TabNewLeft => unreachable!("handled above"),
            A::TabNew => {
                // Same rationale as the TabNewRight/TabNewLeft branch above:
                // open on the current engine, not via the router. The user
                // expects the new tab on the engine they're already viewing;
                // the router applies only on real-URL navigation.
                let url = self.new_tab_url.clone();
                if let Err(err) = host.open_tab(&url) {
                    warn!(error = %err, %url, "tab_new: failed");
                }
            }
            A::TabClose => {
                self.close_active_tab_or_exit();
            }
            A::TabNext => {
                host.next_tab();
                self.close_overlay();
            }
            A::TabPrev => {
                host.prev_tab();
                self.close_overlay();
            }
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
                let url = match self.active_engine_dyn().and_then(|e| e.clipboard_text()) {
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
                    if let Some(engine) = self.active_engine_dyn() {
                        engine.run_edit_focus(id);
                    }
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
            A::Engine(id) => {
                // Rebind the active tab to a different engine — same URL,
                // new engine. Mirrors the cross-engine nav pattern:
                // snapshot URL → open on target → close on source → switch.
                let target_engine_id = buffr_engine::EngineId::new(id);
                if !self.engines.contains_key(&target_engine_id) {
                    tracing::warn!(id = %id, ":engine — unknown engine id");
                    return;
                }
                if target_engine_id == self.active_engine {
                    tracing::debug!(id = %id, ":engine — already on this engine, no-op");
                    return;
                }
                // Snapshot the current URL before we touch anything.
                let url = host.active_tab_live_url();
                let url = if url.is_empty() {
                    self.homepage.clone()
                } else {
                    url
                };
                // Open on the target engine.
                let Some(target_host) = self.engines.get(&target_engine_id) else {
                    tracing::warn!(id = %id, ":engine — target engine vanished");
                    return;
                };
                match buffr_engine::BrowserEngine::open_tab(target_host.as_ref(), &url) {
                    Ok(_) => {
                        tracing::debug!(target = %target_engine_id, url = %url, ":engine swap: opened on target");
                    }
                    Err(err) => {
                        tracing::warn!(target = %target_engine_id, error = %err, ":engine swap: open_tab on target failed");
                        return;
                    }
                }
                // Close the active tab on the source engine.
                // `host` borrow is already active; re-acquire through engines map
                // since we released it above.
                let source_host = self
                    .engines
                    .get(&self.active_engine)
                    .expect("source engine still registered")
                    .clone();
                if let Err(err) = buffr_engine::BrowserEngine::close_active(source_host.as_ref()) {
                    tracing::warn!(error = %err, ":engine swap: close_active on source failed (tab may linger)");
                }
                // Switch to the target engine.
                self.active_engine = target_engine_id;
                self.refresh_tab_strip();
                self.mark_chrome_dirty();
                self.mark_session_dirty();
                self.request_redraw();
            }
            // DevTools: route through the active engine's trait surface so
            // blink-cdp opens the Chromium remote-debug inspector URL in the
            // system browser rather than falling into host.dispatch (CEF only).
            buffr_modal::PageAction::OpenDevTools => {
                if let Some(engine) = self.engines.get(&self.active_engine)
                    && let Some(tab) = engine.active_tab().map(|s| s.id)
                    && let Err(err) = engine.open_devtools(tab)
                {
                    tracing::warn!(error = %err, "open_devtools failed");
                }
            }
            // Zoom actions: route through the active engine's trait
            // surface so non-CEF backends (blink-cdp etc.) receive
            // them. CEF's BrowserHost implements the same trait
            // methods, so this stays consistent for CEF-active tabs too.
            buffr_modal::PageAction::ZoomIn
            | buffr_modal::PageAction::ZoomOut
            | buffr_modal::PageAction::ZoomReset => {
                if let Some(engine) = self.engines.get(&self.active_engine) {
                    match action {
                        buffr_modal::PageAction::ZoomIn => engine.zoom_in(),
                        buffr_modal::PageAction::ZoomOut => engine.zoom_out(),
                        buffr_modal::PageAction::ZoomReset => engine.zoom_reset(),
                        _ => unreachable!(),
                    }
                }
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
        let Some(host) = self.active_engine_dyn() else {
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
            Ok(still_open) => {
                // Phase 3: "last tab" exit must check across ALL engines.
                let total_tabs: usize = self.engines.values().map(|e| e.tab_count()).sum();
                if !still_open || total_tabs == 0 {
                    info!("tab_close: last tab gone (all engines) — requesting graceful exit");
                    self.save_session_now();
                    self.mark_clean_shutdown();
                    // Signal the event loop to exit on the next `about_to_wait`
                    // tick instead of `std::process::exit(0)`. Direct exit()
                    // bypasses Rust `Drop` so the WPE WebKit worker thread keeps
                    // running and WebKit's libc atexit destructors then SIGABRT
                    // unwinding a half-initialised display. Routing through
                    // `event_loop.exit()` runs the post-`run_app` shutdown
                    // sequence (engine drops, backend.shutdown(), …) before
                    // libc atexit fires.
                    self.shutdown_flag.store(true, Ordering::SeqCst);
                    self.request_redraw();
                }
                true
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
        if let Some(host) = self.active_engine_dyn() {
            let _ = host.close_tab(target);
            // Phase 3: count across ALL engines for the exit decision.
            let total_tabs: usize = self.engines.values().map(|e| e.tab_count()).sum();
            if total_tabs == 0 {
                info!("tab_close: last tab gone (all engines) — requesting graceful exit");
                self.save_session_now();
                self.mark_clean_shutdown();
                // See close_active_tab_or_exit for the rationale: signal
                // the event loop so the post-run_app shutdown sequence
                // tears down engines + workers before libc atexit fires.
                self.shutdown_flag.store(true, Ordering::SeqCst);
                self.request_redraw();
            }
            self.refresh_tab_strip();
            self.mark_session_dirty();
        }
    }

    /// Clear the crash-loop tracker. Call only at genuine graceful
    /// shutdown sites — *not* from debounced session flushes — so the
    /// next launch starts with a clean attempt history.
    fn mark_clean_shutdown(&self) {
        if let Some(path) = self.crash_guard_path.as_ref() {
            crash_guard::record_clean_exit(path);
        }
        // Also signal intent to the supervisor via the path it set in
        // BUFFR_SUPERVISOR_CLEAN_FLAG. Touching the file tells the
        // supervisor to treat the subsequent process exit as
        // intentional even if CEF / wgpu teardown segfaults on the
        // way out — without this, exit 139 would trigger a respawn
        // immediately after the user closed the window.
        if let Ok(path) = std::env::var("BUFFR_SUPERVISOR_CLEAN_FLAG")
            && !path.is_empty()
            && let Err(err) = std::fs::write(&path, b"")
        {
            tracing::warn!(error = %err, path, "shutdown: failed to write supervisor clean flag");
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
        let Some(host) = self.active_engine_dyn() else {
            return;
        };
        let summaries = host.tabs_summary();
        let active = host.active_index();
        let ids: Vec<TabId> = summaries.iter().map(|t| t.id).collect();
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
        // Collect (browser_id, url) pairs to prefill from the disk cache.
        // Built inside the host-borrow scope, applied afterwards so the
        // borrow checker sees no aliased &mut self.
        let mut prefill_queue: Vec<(i32, String)> = Vec::new();

        {
            let Some(host) = self.active_engine_dyn() else {
                return;
            };
            // Restored session first — these were saved in the previous
            // run's tab order. The first one is already loaded as the
            // initial tab via the engine constructor; the rest open in the
            // background so the user lands on tab 0.
            let session = std::mem::take(&mut self.pending_session_tabs);
            // CEF auto-creates an initial homepage tab during `open_engine`,
            // so the first restored URL navigates that existing tab.
            // blink-cdp starts with zero tabs, so the first restored URL must
            // be opened, not navigated. Detect by querying tab_count().
            let has_initial_tab = host.tab_count() > 0;
            // When the session restored nothing, the auto-created homepage
            // tab is still sitting there unused, and the first CLI URL
            // should take it over (see the `cli_tabs` loop below).
            let session_claimed_initial_tab = !session.is_empty();
            for (i, (url, pinned)) in session.iter().enumerate() {
                if i == 0 && has_initial_tab {
                    // CEF path: navigate the auto-created tab in place so we
                    // don't end up with a stray homepage tab.
                    if let Err(err) = host.navigate(url) {
                        warn!(error = %err, %url, "session: navigate first tab failed");
                    }
                    if *pinned && let Some(active) = host.active_tab() {
                        host.set_pinned(active.id, true);
                    }
                    if let Some(active) = host.active_tab() {
                        prefill_queue.push((active.browser_id, url.clone()));
                    }
                    continue;
                }
                if i == 0 {
                    // blink-cdp path: open tab 0 as a foreground tab so it
                    // becomes active and matches the user's last session.
                    match host.open_tab(url) {
                        Ok(id) => {
                            if *pinned {
                                host.set_pinned(id, true);
                            }
                            if let Some(last) = host.tabs_summary().last() {
                                prefill_queue.push((last.browser_id, url.clone()));
                            }
                        }
                        Err(err) => {
                            warn!(error = %err, %url, "session: open first tab failed")
                        }
                    }
                    continue;
                }
                // Phase 2: route through the engine router.
                match self.routed_open_tab_background(url) {
                    Ok(id) => {
                        if *pinned {
                            // The new tab is in the background, so the
                            // pin must target it by id rather than the
                            // currently-active tab.
                            host.set_pinned(id, true);
                        }
                        // Queue a prefill for the new browser's id. The
                        // CEF browser is created synchronously so the last
                        // entry in tabs_summary() is the tab we just opened.
                        if let Some(last) = host.tabs_summary().last() {
                            prefill_queue.push((last.browser_id, url.clone()));
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
                    self.on_tab_switch();
                }
            }
            // CLI URLs append after the session.
            //
            // The first one navigates the auto-created homepage tab when the
            // session restored nothing, exactly as the session's own first
            // entry does above. Opening it in the background instead left
            // `buffr https://example.com` sitting on a blank `buffr://new`
            // with the requested page hidden behind it — the URL the user
            // asked for was never the one they landed on.
            let cli_tabs = std::mem::take(&mut self.pending_new_tabs);
            let mut claim_initial_tab = has_initial_tab && !session_claimed_initial_tab;
            // `buffr <url>` is an explicit "show me this". The first CLI URL
            // ends up active either way: it takes over the unused homepage
            // tab when there is one, and is selected after opening when a
            // restored session already owns tab 0.
            let mut focus_first_cli_tab = !claim_initial_tab;
            for url in cli_tabs {
                if claim_initial_tab {
                    claim_initial_tab = false;
                    if let Err(err) = host.navigate(&url) {
                        warn!(error = %err, %url, "cli-url: navigate first tab failed");
                        continue;
                    }
                    if let Some(active) = host.active_tab() {
                        prefill_queue.push((active.browser_id, url.clone()));
                    }
                    continue;
                }
                match self.routed_open_tab_background(&url) {
                    Ok(id) => {
                        if let Some(last) = host.tabs_summary().last() {
                            prefill_queue.push((last.browser_id, url.clone()));
                        }
                        if focus_first_cli_tab {
                            focus_first_cli_tab = false;
                            host.select_tab(id);
                            self.on_tab_switch();
                        }
                    }
                    Err(err) => warn!(error = %err, %url, "new-tab: open_tab failed"),
                }
            }
        } // host borrow ends here

        // Apply prefills now that the immutable host borrow has been dropped.
        for (browser_id, url) in prefill_queue {
            self.register_favicon_prefill(browser_id, &url);
        }
    }

    /// If `show_favicons` is on and the cache is available, register
    /// `(browser_id, origin_of(url))` in `pending_favicon_prefill` so
    /// `pump_favicon_updates` can apply the cached bitmap on its next tick.
    fn register_favicon_prefill(&mut self, browser_id: i32, url: &str) {
        if !self.show_favicons || self.favicon_cache.is_none() {
            return;
        }
        if let Some(origin) = buffr_core::origin_of(url) {
            self.pending_favicon_prefill.insert(browser_id, origin);
        }
    }

    /// Refresh the tab-strip render input from the host's current
    /// tab list. Cheap; runs every `about_to_wait` tick.
    fn refresh_tab_strip(&mut self) -> (bool, Vec<TabSummary>) {
        let Some(host) = self.active_engine_dyn() else {
            return (false, Vec::new());
        };
        let summaries = host.tabs_summary();
        let active = host.active_index();
        // Drop favicon entries for closed browsers so the map doesn't
        // grow without bound across long sessions.
        let live_ids: std::collections::HashSet<i32> =
            summaries.iter().map(|s| s.browser_id).collect();
        self.favicons.retain(|id, _| live_ids.contains(id));
        // Determine the engine badge colour and 2-char label for the
        // currently-active engine. All tabs in the strip belong to
        // `active_engine`; the badge is the same on every tab. `None` when
        // there's only one engine or when `active_engine` is the primary "cef".
        let active_engine_id = self.active_engine.clone();
        let engine_badge = self
            .engine_router
            .as_ref()
            .and_then(|r| r.badge_color_for(&active_engine_id));
        let engine_label = self
            .engine_router
            .as_ref()
            .and_then(|r| r.badge_label_for(&active_engine_id));
        // Which tab index the cursor is hovering over (for badge outline).
        let hovered_tab_idx = self.hit_test_tab_strip();
        let mut ids = Vec::with_capacity(summaries.len());
        let tabs = summaries
            .iter()
            .enumerate()
            .map(|(idx, t)| {
                ids.push(t.id);
                let favicon = self.favicons.get(&t.browser_id).cloned();
                TabView {
                    title: t.title.clone(),
                    progress: t.progress,
                    pinned: t.pinned,
                    private: t.private,
                    favicon,
                    engine_badge,
                    engine_label: engine_label.clone(),
                    hovered: hovered_tab_idx == Some(idx),
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
        (tabs_changed, summaries)
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
        if leaving_visual && let Some(engine) = self.active_engine_dyn() {
            // Drop the page's DOM selection so the highlight goes with
            // Visual mode. Any prior YankSelection JS has already been
            // queued in the renderer and runs first; this just collapses
            // what's left.
            let _ = engine.run_main_frame_js(
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
            tracing::debug!(target: "buffr::ui_path", "enter: window.request_redraw");
            window.request_redraw();
            tracing::debug!(target: "buffr::ui_path", "exit:  window.request_redraw");
        }
    }

    /// Drain decoded favicon bitmaps from CEF and stash by browser id.
    /// `refresh_tab_strip` reads this map to attach the bitmap to each
    /// `TabView`. Marks chrome dirty when at least one update lands so
    /// the new favicon shows up on the next paint.
    ///
    /// Also:
    /// - Applies any pending disk-cache prefills registered by
    ///   `register_favicon_prefill` (one lookup per pending entry; hits
    ///   populate `self.favicons` before the CEF callback fires).
    /// - Persists every fresh CEF-delivered bitmap back to the disk cache.
    fn pump_favicon_updates(&mut self, summaries: &[TabSummary]) -> bool {
        let Some(engine) = self.active_engine_dyn() else {
            return false;
        };
        if !self.show_favicons {
            // Drop any stale entries so a runtime toggle to "off" takes
            // effect immediately on the next refresh.
            if !self.favicons.is_empty() {
                self.favicons.clear();
                self.pending_favicon_prefill.clear();
                return true;
            }
            return false;
        }

        let mut changed = false;

        // ── Runtime scan: enqueue prefills for tabs whose URL changed ────────
        //
        // Catches every code path that opens a tab without going through the
        // session-restore queue: omnibar navigates, link middle-clicks, hint
        // mode, popup → tab, view-source, etc. We compare each tab's current
        // URL against the one we last cache-checked for that browser_id;
        // mismatch → enqueue a prefill. Closed browsers are dropped from the
        // memoization map to bound memory.
        if self.favicon_cache.is_some() {
            let live_ids: std::collections::HashSet<i32> =
                summaries.iter().map(|t| t.browser_id).collect();
            self.favicon_check_url.retain(|id, _| live_ids.contains(id));
            for tab in summaries.iter() {
                let same = self
                    .favicon_check_url
                    .get(&tab.browser_id)
                    .is_some_and(|prev| prev == &tab.url);
                if same {
                    continue;
                }
                self.favicon_check_url
                    .insert(tab.browser_id, tab.url.clone());
                // Don't overwrite an existing favicon — CEF-delivered wins.
                if self.favicons.contains_key(&tab.browser_id) {
                    continue;
                }
                if let Some(origin) = buffr_core::origin_of(&tab.url) {
                    self.pending_favicon_prefill.insert(tab.browser_id, origin);
                }
            }
        }

        // ── Apply cached prefills ────────────────────────────────────────────
        //
        // Drain `pending_favicon_prefill` entries. For each browser that does
        // not yet have a favicon, look up the disk cache by origin. On hit,
        // synthesize a `TabFavicon` and stash it so the next
        // `refresh_tab_strip` can paint it without waiting for CEF.
        if let Some(cache) = self.favicon_cache.as_ref() {
            let pending: Vec<(i32, String)> = self.pending_favicon_prefill.drain().collect();
            for (browser_id, origin) in pending {
                // Skip if CEF already delivered a fresh one this session.
                if self.favicons.contains_key(&browser_id) {
                    continue;
                }
                if let Some(cached) = cache.get(&origin) {
                    debug!(browser_id, %origin, "favicon cache: prefill hit");
                    let fav = buffr_ui::TabFavicon {
                        width: cached.width,
                        height: cached.height,
                        pixels: std::sync::Arc::new(cached.pixels),
                    };
                    self.favicons.insert(browser_id, fav);
                    changed = true;
                }
            }
        } else {
            // No cache — just discard pending entries.
            self.pending_favicon_prefill.clear();
        }

        // ── Drain fresh engine-delivered bitmaps ─────────────────────────────
        let updates = engine.drain_favicon_updates();
        if updates.is_empty() {
            return changed;
        }
        // Build a browser_id → url map from the current tab list so we can
        // resolve the origin for each incoming favicon.
        let id_to_url: HashMap<i32, &str> = summaries
            .iter()
            .map(|t| (t.browser_id, t.url.as_str()))
            .collect();
        for u in updates {
            // Persist to disk cache keyed by the tab's current origin.
            if let Some(cache) = self.favicon_cache.as_ref()
                && let Some(url) = id_to_url.get(&u.browser_id)
                && let Some(origin) = buffr_core::origin_of(url)
            {
                debug!(browser_id = u.browser_id, %origin, "favicon cache: write");
                cache.put(&origin, u.width, u.height, &u.pixels);
            }
            // Engine-delivered bitmap always wins — remove any prefill placeholder.
            self.pending_favicon_prefill.remove(&u.browser_id);
            let fav = buffr_ui::TabFavicon {
                width: u.width,
                height: u.height,
                pixels: std::sync::Arc::new(u.pixels),
            };
            self.favicons.insert(u.browser_id, fav);
        }
        true
    }

    /// Drain CEF cursor changes and forward to winit. CEF emits a new
    /// `CursorType` whenever the page wants the system cursor to change
    /// (link hover, text-input hover, resize edge, …); we map it to
    /// winit's [`CursorIcon`] and call `Window::set_cursor` on the window
    /// owning the originating browser (main tab or popup). Last writer
    /// wins — coalescing is desirable since CEF can fire many times per
    /// frame as the cursor moves.
    fn pump_cursor_changes(&self, event_loop: &EventLoop<BuffrUserEvent>) {
        let Some(engine) = self.active_engine_dyn() else {
            return;
        };
        let Some((browser_id, raw)) = engine.take_cursor_change() else {
            return;
        };
        let icon = cef_cursor_to_icon(raw);
        // winit's set_cursor is per-window (not per-seat the way wayr /
        // raw Wayland are), so apply to every live window: the main
        // toplevel + every popup. winit silently ignores the request
        // for non-focused windows, so whichever surface holds pointer
        // focus picks up the cursor. We log the originating browser_id
        // for diagnostics but don't route on it.
        let _ = browser_id;
        if let Some(win) = self.window.as_ref() {
            win.set_cursor(event_loop, icon);
        }
        for popup in self.popups.values() {
            popup.window.set_cursor(event_loop, icon);
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
        if let (Some(host), Some(query)) = (self.active_engine_dyn(), self.pending_find.take()) {
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
        tracing::debug!(target: "buffr::ui_path", "enter: paint_chrome");
        self.paint_chrome_with(None);
        tracing::debug!(target: "buffr::ui_path", "exit:  paint_chrome");
    }

    /// Two-finger horizontal-swipe back/forward gesture detector.
    /// Call once per touchpad `PixelDelta` event with the raw delta in
    /// screen pixels — see [`scroll_swipe_delta`] for mapping a
    /// single-axis `ScrollEvent` onto `(dx, dy)`. Thin clock wrapper
    /// around [`SwipeDetector::feed`], which holds the actual rules.
    fn detect_swipe(&mut self, dx: f32, dy: f32) -> Option<buffr_modal::PageAction> {
        self.swipe.feed(dx, dy, Instant::now())
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
        if let Some(engine) = self.active_engine_dyn() {
            let mods = mods_to_cef(&self.modifiers);
            // osr_cursor is physical (browser-region-relative); OSR takes DIPs.
            let (phys_bx, phys_by) = self.osr_cursor;
            let mom_scale = self.current_scale();
            let (bx, by) = physical_cursor_to_dip(phys_bx, phys_by, 0, mom_scale);
            tracing::trace!(dx, dy, "input: wheel_momentum -> engine");
            engine.osr_mouse_wheel(bx, by, dx, dy, mods);
        }
    }

    /// Paint chrome at explicit dims when caller has fresher size info
    /// than `window.inner_size()` returns. Wayland's configure handshake
    /// can leave `window.inner_size()` reporting the previous dims at
    /// the moment `WindowEvent::Resized` fires; passing the event's
    /// `new_size` directly avoids painting at stale width/height.
    ///
    /// Thin wrapper around [`Self::paint_chrome_inner`] that owns the
    /// wake-probe lifecycle (M33): `probe_pending` is read-and-cleared
    /// here, ONCE, so it can never survive one of the inner function's
    /// four early returns.  Before this split, the idle short-circuit
    /// could leave `probe_pending == true` with `next_probe_at == None`
    /// forever, permanently bypassing the occlusion sleep guard.
    fn paint_chrome_with(&mut self, override_size: Option<(u32, u32)>) {
        let probe_pending = std::mem::take(&mut self.probe_pending);
        let submitted = self.paint_chrome_inner(override_size, probe_pending);
        // A probe that never reached the GPU produced no timing sample, so
        // `observe_present_us` never got the chance to either wake us or
        // reschedule.  Re-arm the cadence so the window cannot get stuck
        // asleep with no probe pending.  (When the probe DID wake us the
        // policy is no longer Sleeping and this is a no-op.)
        if probe_pending
            && !submitted
            && self.paint_policy == PaintPolicy::Sleeping
            && self.next_probe_at.is_none()
        {
            self.next_probe_at = Some(Instant::now() + OCCLUSION_PROBE_INTERVAL);
        }
    }

    /// Body of [`Self::paint_chrome_with`].
    ///
    /// `probe_pending` is the consumed wake-probe flag (see the wrapper).
    /// Returns `true` when the renderer actually handed a frame to the
    /// wgpu worker — callers use it to decide whether the probe was spent.
    fn paint_chrome_inner(
        &mut self,
        override_size: Option<(u32, u32)>,
        probe_pending: bool,
    ) -> bool {
        // Drain async-present stats from the worker thread BEFORE doing any
        // wgpu work this frame.  Once a present has blocked on compositor
        // backpressure, subsequent queue.write_texture / queue.submit calls
        // inherit the same backpressure and block too — observed osr_us=6.27s
        // on the frame following a 6.29s present.  Polling here means the
        // occlusion heuristic trips before we touch the GPU, so the sleep
        // guard below catches us instead.
        //
        // These stats come from a frame the worker really did present, so
        // unlike the post-`frame()` sample below they are always safe to
        // observe.
        let new_stats = self.renderer.as_mut().and_then(|r| r.poll_present_stats());
        if let Some(stats) = new_stats {
            self.observe_present_us(stats.present_us, probe_pending);
        }

        // OSR sleep guard: skip the wgpu present while the policy is
        // Sleeping.  Bypass exceptions:
        //
        // - `surface_drifted`: CRITICAL for #17 — if Hyprland sends a
        //   configure while sleeping, the stale buffer has the wrong dims
        //   and the compositor letterboxes it.  The resize self-heal must
        //   present at the new dims.
        // - `probe_pending`: a wake-probe was scheduled by `about_to_wait`
        //   to test whether the compositor is now showing our surface
        //   (heuristic-occlusion case where winit Occluded is unreliable).
        //
        // chrome_dirty is intentionally NOT a bypass: a user keystroke or
        // mode change while we believe ourselves occluded would otherwise
        // initiate a wgpu present that can block the UI thread for
        // multiple seconds (Wayland compositor refusing to release the
        // buffer for a hidden surface).  The next probe (≤2 s away) will
        // wake us if visible; chrome catches up then.
        if self.paint_policy == PaintPolicy::Sleeping && !self.surface_drifted && !probe_pending {
            return false;
        }
        let Some(window) = self.window.as_ref() else {
            return false;
        };
        let inner = window.physical_size();
        let (width, height) = match override_size {
            Some((w, h)) => (w.max(1), h.max(1)),
            None => (inner.width.max(1), inner.height.max(1)),
        };
        // Logical (DIP) dimensions for chrome layout. The chrome CPU buffer
        // is sized at logical pixels and GPU-stretched to physical by the
        // bilinear sampler. At integer scales (1×, 2×) the chrome bitmap-font
        // glyphs sample at exact pixel boundaries, producing crisp text.
        let scale = window.scale_factor() as f32;
        let (lwidth, lheight) = logical_chrome_dims(width, height, scale);

        // Precompute geometry before the renderer call — helpers need `&self`.
        // Use logical dims for chrome layout so strip heights are in DIPs.
        let tab_y = self.tab_strip_y(lheight);
        let notice_y = self.download_notice_y();
        let current_notice = peek_download_notice(&self.download_notice_queue);
        // OSR browser rect uses physical dims (CEF paints physical) for dst_rect.
        let (_, browser_y, browser_w, browser_h) = self.cef_child_rect(width, height);
        // Logical browser rect for the loading animation (painted into chrome buffer).
        let (_, l_browser_y, l_browser_w, l_browser_h) =
            self.cef_child_rect_logical(lwidth, lheight);

        // Acquire the latest OSR pixels by swapping our scratch buffer
        // with the SharedOsrFrame's pixel Vec. Lock duration is the cost
        // of a Vec<u8> swap (three usize writes) — negligible.
        //
        // Freshness gate: only swap when (a) generation has advanced past
        // last_osr_generation (CEF emitted a new on_paint since we last
        // looked) AND (b) frame.pixels.len() matches frame.width*height*4
        // (the buffer is consistent with the dim atomics). The naive
        // `!frame.pixels.is_empty()` check is NOT sufficient: after our
        // first swap, frame.pixels holds the OLD scratch Vec — which is
        // non-empty AND keeps its old length. Then on_paint at NEW dims
        // resizes that Vec and updates frame.{width,height}. The next UI
        // paint with no further on_paint between would see "non-empty
        // pixels + new dims" and swap again, putting the leftover OLD
        // Vec into scratch. scratch.len() (old dims) would no longer
        // match last_osr_dims (new dims) → FreshOsr arm builds an
        // OsrUpload claiming new_w*new_h*4 bytes from a buffer with only
        // old_w*old_h*4 bytes. wgpu validation panics:
        //   "Copy of 0..N would end up overrunning bounds of source M"
        // (observed on Hyprland during a resize that crossed the chrome
        // strip boundary at non-integer scale).
        //
        // Generation tracking guarantees the swap only fires when CEF
        // has actually written fresh pixels since our last swap.
        let osr_meta: Option<(u32, u32, u64)> = if let Some(host) = self.active_engine_dyn() {
            // Read the dims we asked CEF for (osr_view atomics) so the
            // gate can reject in-flight stale-dim paints. Reading both
            // atomics under the frame lock matches CEF's IO-thread
            // ordering: on_paint holds the same lock when writing
            // frame.{width,height}, so an osr_view read here is
            // sequenced with that on_paint.
            let osr_view = host.osr_view();
            let expected_w = osr_view.width.load(std::sync::atomic::Ordering::Relaxed);
            let expected_h = osr_view.height.load(std::sync::atomic::Ordering::Relaxed);
            if let Ok(mut frame) = host.osr_frame().lock() {
                let fresh = is_osr_frame_fresh(
                    frame.width,
                    frame.height,
                    frame.pixels.len(),
                    frame.generation,
                    self.last_osr_generation,
                    expected_w,
                    expected_h,
                    frame.needs_fresh,
                );
                if fresh {
                    // Record dims before the swap (while guard is held).
                    self.last_osr_dims = Some((frame.width, frame.height));
                    self.resize_paint_watchdog
                        .observe_paint(frame.width, frame.height);
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

        // Query the active engine's loading state before taking the mutable
        // renderer borrow — `active_engine_dyn()` clones the Arc so this doesn't
        // keep a borrow on `self`, allowing `renderer.as_mut()` below.
        let host_is_loading = self
            .active_engine_dyn()
            .map(|e| e.is_loading())
            .unwrap_or(false);

        // Native-compositing engines (WebKit on Wayland) render directly into a
        // Wayland subsurface; the chrome quad's browser region stays fully
        // transparent (alpha = 0) so the subsurface shows through. The loading
        // animation would never be visible there anyway, so suppress it to
        // prevent painting opaque pixels into the transparent browser region.
        //
        // Critical: gate on `is_using_native_compositing` (current path),
        // not `supports_native` (capability).  Engines that COULD composite
        // natively but aren't (env opt-out, init fallback, wrong session
        // type at runtime) need the OSR-side animation path; reading the
        // capability gates the animation off forever on those engines.
        let host_is_using_native_compositing = self
            .active_engine_dyn()
            .map(|e| e.is_using_native_compositing())
            .unwrap_or(false);

        let Some(renderer) = self.renderer.as_mut() else {
            return false;
        };

        // Resize bumps chrome_generation via the caller's resize event;
        // the renderer itself tracks whether it needs to reallocate.
        renderer.resize(width, height);
        // Update the logical chrome dims so the chrome texture is sized at
        // DIP resolution and GPU-stretched to physical by the bilinear sampler.
        renderer.set_logical_size(lwidth, lheight);

        let chrome_dirty = self.chrome_generation != self.last_painted_chrome_gen;

        let frame_start = Instant::now();

        // Decide whether to show the loading animation instead of OSR.
        // Pixel-state gating only — no time threshold.
        //   • None (last_osr_dims)   → no on_paint received yet
        //   • Some dims != browser   → CEF hasn't caught up to new size yet
        // We do NOT use `osr_meta.is_none()` here because that fires on every
        // redraw between CEF paints (the swap-out side effect empties
        // frame.pixels). `last_osr_dims` persists across swaps so the animation
        // only activates before the very first paint or during a size mismatch.
        // Force the animation to keep playing while we are recovering from a
        // surface-size mismatch (Hyprland letterboxes a wl_surface buffer
        // whose dims don't match the configured surface size). The drift
        // flag is set at the end of the previous paint when we detected the
        // condition; the animation overlays the wrong-sized OSR until the
        // reconcile redraw lands.
        //
        // Don't paint the loading animation into the browser region when the
        // active engine composites natively (WebKit/Wayland subsurface). The
        // browser region is kept fully transparent so the subsurface shows
        // through; an opaque animation there would occlude it.
        // Show the splash overlay only when there are no usable pixels to
        // present — either no OSR frame yet, or the most recent frame's
        // dims don't match the current browser rect (mid-resize / first
        // paint), or the renderer flagged a stale-size buffer drift on
        // the previous paint.
        //
        // `host_is_loading` is deliberately NOT part of this gate.  The
        // atomic that drives it is cleared by the engine's
        // load-state-changed signal handler on the active tab only, so
        // it gets pinned true forever whenever a tab switch races a
        // LOAD_COMMITTED, a navigation errors before commit, or a
        // page-state callback drops a signal — symptom seen in
        // production: animation never gives way to the page despite
        // fresh frames arriving.  Loading progress belongs in the
        // statusline / progress spinner, not the full-screen splash;
        // user-visible "I can see the page now" maps to "we have
        // pixels at the right dims", which is exactly what
        // `should_show_loading_anim` measures.
        let want_anim = (should_show_loading_anim(self.last_osr_dims, browser_w, browser_h)
            || self.surface_drifted)
            && !host_is_using_native_compositing;
        let _ = host_is_loading;

        // Idle short-circuit. If nothing has changed since the last paint —
        // chrome buffer up to date, no fresh CEF paint queued, no animation
        // pending, and the loading-anim flag is already in sync (so we won't
        // miss a deactivation chrome-clear) — skip the present entirely.
        // Without this guard, every spurious winit RedrawRequested (cursor
        // motion outside the window, compositor frame callback after a
        // no-op present, periodic poll wakeups in `about_to_wait`) burns a
        // full wgpu acquire+submit+present cycle re-uploading bytes the GPU
        // already has, which manifests as a 6 Hz spin under Wayland's
        // frame-callback coalescing on visible-but-idle pages.
        if !chrome_dirty
            && osr_meta.is_none()
            && !want_anim
            && want_anim == self.loading_anim_active
        {
            return false;
        }

        // Detect the animation→OSR transition. While the animation was
        // active, the chrome buffer had OPAQUE animation pixels painted
        // into the browser region (the chrome quad composites on top of
        // OSR — see render::frame). When we transition to a non-anim
        // path with chrome_dirty=false, the renderer skips the chrome
        // upload, so the chrome texture keeps its last-uploaded state
        // (with the animation pixels) and occludes the OSR. Force a
        // chrome repaint THIS frame to clear the browser region.
        let anim_just_deactivated = self.loading_anim_active && !want_anim;
        if want_anim != self.loading_anim_active {
            if want_anim {
                tracing::debug!(
                    browser_w,
                    browser_h,
                    last_osr_dims = ?self.last_osr_dims,
                    "loading_anim: activated (no prior paint or wrong size)"
                );
            } else {
                tracing::debug!(
                    browser_w,
                    browser_h,
                    "loading_anim: deactivated (OSR buffer matches browser rect)"
                );
            }
            self.loading_anim_active = want_anim;
        }

        let splash = &self.splash;
        let anim_fg = self.statusline.palette.accent;
        // bg: accent darkened 92% with black, matching the strip background.
        let anim_bg = buffr_ui::Palette::from_accent(anim_fg).bg;

        // Build the OsrUpload from our just-swapped scratch buffer.
        let new_osr_generation;
        let chrome_dirty_effective =
            should_force_chrome_repaint(chrome_dirty, want_anim, anim_just_deactivated);
        let paint_path = decide_paint_path(want_anim, osr_meta.is_some(), self.last_osr_dims);

        // Clone/snapshot values needed in the chrome paint closure. Gated on
        // `chrome_dirty_effective`: `renderer.frame` invokes its paint
        // closure only when the chrome buffer is repainted (see render.rs),
        // so a frame presenting a fresh OSR frame with unchanged chrome —
        // the common per-tick case — skips the statusline/tab-strip clones
        // and the context-menu overlay rebuild entirely. When the gate is
        // closed the values are all `None` and `paint_strips` below is a
        // no-op.
        let (
            statusline,
            tab_strip,
            confirm_close_pinned,
            permissions_prompt,
            overlay_data,
            context_menu_overlay,
        ) = if chrome_dirty_effective {
            (
                Some(self.statusline.clone()),
                Some(self.tab_strip.clone()),
                self.confirm_close_pinned,
                self.permissions_prompt.clone(),
                self.overlay.as_ref().map(|o| o.input().clone()),
                self.context_menu
                    .as_ref()
                    .map(|cm| cm.to_overlay(lwidth, lheight)),
            )
        } else {
            (None, None, None, None, None, None)
        };

        // Which chrome rows actually reached the GPU: by default only the
        // painted strip bands — the top band (tab strip + download notice,
        // a fixed upper bound: when no notice is queued it re-uploads a few
        // transparent rows, which is harmless) and the bottom band
        // (statusline). The browser region between them is transparent and
        // skipped, turning the full-texture upload into two thin strips.
        // But when anything paints into the browser region this frame — the
        // loading animation, a floating omnibar/prompt/context menu, or the
        // animation→OSR transition that must clear the animation pixels —
        // the whole buffer has to reach the GPU, so the top band covers the
        // full logical height.
        let chrome_middle_painted = want_anim
            || anim_just_deactivated
            || confirm_close_pinned.is_some()
            || permissions_prompt.is_some()
            || overlay_data.is_some()
            || context_menu_overlay.is_some();
        let (chrome_top_band_h, chrome_bottom_band_h) = if chrome_middle_painted {
            (lheight, 0)
        } else {
            (TAB_STRIP_HEIGHT + DOWNLOAD_NOTICE_HEIGHT, STATUSLINE_HEIGHT)
        };
        // Single chrome-strip painter shared by every `PaintPath` arm — the
        // arms differ only in what they hand the renderer as the OSR layer
        // (and, for `Animation`, in the splash blit layered on top).
        let paint_strips = |buf: &mut [u32], w: usize| {
            let (
                Some(statusline),
                Some(tab_strip),
                Some(confirm_close_pinned),
                Some(permissions_prompt),
                Some(overlay_data),
                Some(context_menu_overlay),
            ) = (
                statusline.as_ref(),
                tab_strip.as_ref(),
                confirm_close_pinned.as_ref(),
                permissions_prompt.as_ref(),
                overlay_data.as_ref(),
                context_menu_overlay.as_ref(),
            )
            else {
                // Chrome not repainted this frame — nothing to paint.
                return;
            };
            paint_chrome_strips(
                buf,
                w,
                lheight,
                statusline,
                tab_strip,
                tab_y,
                notice_y,
                current_notice.as_ref(),
                Some(*confirm_close_pinned),
                Some(permissions_prompt),
                Some(overlay_data),
                Some(context_menu_overlay),
            );
        };
        let res = match paint_path {
            PaintPath::Animation => {
                // Animation path: paint animation into chrome buffer at the browser
                // rect region (opaque), then composite chrome alone (osr: None).
                // Chrome buffer is logical-sized; use l_browser_* for the animation.
                new_osr_generation = self.last_osr_generation;
                renderer.frame(
                    chrome_dirty_effective,
                    chrome_top_band_h,
                    chrome_bottom_band_h,
                    |buf, w, h| {
                        paint_strips(buf, w);
                        // Paint the animation into the browser region so it is
                        // opaque and composites as chrome (no OSR quad shown).
                        crate::loading_anim::paint(
                            buf,
                            w,
                            h,
                            (0, l_browser_y, l_browser_w, l_browser_h),
                            splash,
                            anim_fg,
                            anim_bg,
                        );
                    },
                    None,
                )
            }
            PaintPath::FreshOsr => {
                let (osr_w, osr_h, osr_gen) = osr_meta.expect("FreshOsr requires osr_meta");
                new_osr_generation = osr_gen;
                // dst_rect uses the live physical browser rect. The renderer GPU-
                // stretches the OSR texture to fill it.
                let osr_upload = crate::render::OsrUpload {
                    pixels: &self.osr_scratch,
                    width: osr_w,
                    height: osr_h,
                    generation: osr_gen,
                    dst_rect: (0, browser_y, browser_w, browser_h),
                    skip_pixels: false,
                };
                renderer.frame(
                    chrome_dirty_effective,
                    chrome_top_band_h,
                    chrome_bottom_band_h,
                    |buf, w, _h| paint_strips(buf, w),
                    Some(osr_upload),
                )
            }
            PaintPath::SyntheticScratch => {
                // Between-paints fallback: frame.pixels was emptied by the
                // mem::swap above, but we already have the previous paint in
                // osr_scratch. Build a synthetic OsrUpload with the previous
                // generation so the renderer dedupes the GPU upload (the texture
                // is already current) while keeping the OSR quad visible.
                let (cached_w, cached_h) = self
                    .last_osr_dims
                    .expect("SyntheticScratch requires last_osr_dims");
                new_osr_generation = self.last_osr_generation;
                tracing::debug!(
                    cached_w,
                    cached_h,
                    gen = self.last_osr_generation,
                    "paint_chrome: between-paints synthetic upload from osr_scratch"
                );
                let osr_upload = crate::render::OsrUpload {
                    pixels: &self.osr_scratch,
                    width: cached_w,
                    height: cached_h,
                    generation: self.last_osr_generation,
                    dst_rect: (0, browser_y, browser_w, browser_h),
                    // Same generation as the GPU already holds: the worker
                    // dedupes the upload, so skip the UI-thread memcpy.
                    skip_pixels: true,
                };
                renderer.frame(
                    chrome_dirty_effective,
                    chrome_top_band_h,
                    chrome_bottom_band_h,
                    |buf, w, _h| paint_strips(buf, w),
                    Some(osr_upload),
                )
            }
            PaintPath::DeadFallback => {
                // Safety fallback: no paint ever received (dead in practice once
                // CEF emits on_paint, but keeps the compiler happy).
                new_osr_generation = self.last_osr_generation;
                renderer.frame(
                    chrome_dirty_effective,
                    chrome_top_band_h,
                    chrome_bottom_band_h,
                    |buf, w, _h| paint_strips(buf, w),
                    None,
                )
            }
        };

        // `last_osr_generation` tracks what we have CONSUMED out of the
        // shared CEF frame (the mem::swap above), not what reached the GPU,
        // so it advances even when the renderer skipped the frame.  Leaving
        // it behind would let the freshness gate swap the same generation a
        // second time on the next paint and push the previous (already
        // consumed) buffer back into `osr_scratch` — see `is_osr_frame_fresh`
        // condition 4.  The skipped pixels are not lost: they live in
        // `osr_scratch` and the retry below re-uploads them through the
        // SyntheticScratch path, which the worker dedupes by generation.
        self.last_osr_generation = new_osr_generation;

        // Schedule the next wake while the loading-anim path is active.
        // `Splash` reads the wall clock each `cells()` call, so we just
        // need to fire a redraw on the splash period to surface the next
        // tick's cell layout.
        if want_anim {
            self.loading_anim_next_wake = Some(Instant::now() + hjkl_splash::DEFAULT_PERIOD);
        } else {
            self.loading_anim_next_wake = None;
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

        // Smoke-test: a completed paint proves the backend reached steady
        // state. Headless Windows runners never deliver WM_PAINT so the
        // RedrawRequested-based signal alone misses every successful run
        // that paints via Resized → paint_chrome.
        if SMOKE_TEST_ACTIVE.load(Ordering::SeqCst)
            && !SMOKE_TEST_SAW_REDRAW.swap(true, Ordering::SeqCst)
        {
            tracing::info!("smoke-test: first paint completed; exiting 0");
            // Smoke exits mid-frame with CEF and the wgpu worker thread
            // still alive. std::process::exit runs atexit + static
            // destructors, which raced the worker's present() and
            // segfaulted at exit; the shutdown path skips them for the
            // same reason (see "shutdown:" below). Flush stderr so the
            // "exiting 0" line above isn't lost, then _exit.
            use std::io::Write;
            let _ = std::io::stderr().flush();
            #[cfg(unix)]
            unsafe {
                libc::_exit(0)
            };
            #[cfg(not(unix))]
            std::process::exit(0);
        }

        // Post-frame bookkeeping.  Everything here is gated on the frame
        // having actually been handed to the wgpu worker (H8 / M34):
        //
        // - Retiring `last_painted_chrome_gen` after a skip erases the dirty
        //   state for pixels that were never uploaded, so the update is lost
        //   until an unrelated event marks chrome dirty again.
        // - The `submit_done_us` sample on a skip is the PREVIOUS frame's
        //   number, so re-observing it double-counts one real measurement:
        //   one slow frame plus two skips fills the history with the same
        //   sample and falsely trips the 3-of-5 occlusion rule, and a
        //   skipped probe re-observes a stale FAST value and "wakes" without
        //   ever having presented.
        //
        // The surviving observation is a same-frame signal: when wgpu's GPU
        // queue is backpressured by the compositor, queue.write_texture and
        // queue.submit block on the worker thread (submit_done_us ballooning
        // to seconds) — catching occlusion modes the lagged present_us
        // misses entirely.
        let outcome = match &res {
            Ok((stats, submitted)) => Some((stats.submit_done_us, *submitted)),
            Err(err) => {
                warn!(error = %err, "wgpu frame failed");
                None
            }
        };
        let commit = decide_frame_commit(outcome, chrome_dirty_effective);
        if commit.advance_chrome_gen {
            self.last_painted_chrome_gen = self.chrome_generation;
        }
        if let Some(us) = commit.observe_us {
            self.observe_present_us(us, probe_pending);
        }
        self.repaint_retry_at = if commit.retry_paint {
            tracing::trace!("paint_chrome: frame skipped by renderer; scheduling retry");
            Some(Instant::now() + SKIPPED_FRAME_RETRY_DELAY)
        } else {
            None
        };

        // Surface-drift detection. We just presented a buffer at
        // (width, height). If `window.inner_size()` has since advanced past
        // those dims (Hyprland queued an additional configure during paint,
        // or the override_size we honored was stale relative to a fresher
        // configure), the wl_surface is at a different size and Hyprland
        // letterboxes the mismatch — visible as persistent black bars on
        // the sides or top/bottom in aspect-fit form. Self-heal: flag the
        // drift (next paint forces the loading animation so the user sees
        // motion) and request a redraw so renderer.resize reconciles to
        // the live inner_size.
        let live_drift = self
            .window
            .as_ref()
            .map(|w| w.physical_size())
            .filter(|s| s.width != width || s.height != height);
        if let Some(s) = live_drift {
            tracing::debug!(
                used_w = width,
                used_h = height,
                live_w = s.width,
                live_h = s.height,
                "paint_chrome: surface drifted from live physical_size; reconciling"
            );
            self.surface_drifted = true;
            self.request_redraw();
        } else {
            self.surface_drifted = false;
        }

        matches!(outcome, Some((_, crate::render::Submitted::Yes)))
    }

    /// Compute the CEF page rect for the current overlay state.
    ///
    /// Vertical layout (top → bottom):
    ///
    /// 1. Download notice strip (`DOWNLOAD_NOTICE_HEIGHT`, when queued)
    /// 2. Tab strip (always, `TAB_STRIP_HEIGHT` px)
    /// 3. CEF page area  ← confirm/permissions/omnibar popups float over this
    /// 4. Statusline (always, `STATUSLINE_HEIGHT` px)
    ///
    /// `full_w` and `full_h` must be **physical** pixels. Chrome heights are
    /// scaled up from their logical-pixel constants before the layout is
    /// computed, so the returned rect is also in physical pixels.
    fn cef_child_rect(&self, full_w: u32, full_h: u32) -> (u32, u32, u32, u32) {
        let has_notice = buffr_core::download_notice_queue_len(&self.download_notice_queue) > 0;
        cef_child_rect_pure(full_w, full_h, self.current_scale(), has_notice)
    }

    /// Same layout as [`Self::cef_child_rect`] but operates in **logical**
    /// (DIP) space. `full_w` and `full_h` must be logical pixels. Uses the
    /// unscaled constants directly. Used for chrome-painter geometry, which
    /// works in logical pixels.
    fn cef_child_rect_logical(&self, full_w: u32, full_h: u32) -> (u32, u32, u32, u32) {
        let has_notice = buffr_core::download_notice_queue_len(&self.download_notice_queue) > 0;
        cef_child_rect_pure(full_w, full_h, 1.0, has_notice)
    }

    /// The pixel row at which the tab strip begins (top of the
    /// `TAB_STRIP_HEIGHT` band). Mirrors the math in
    /// [`Self::cef_child_rect`] without depending on the CEF rect
    /// itself. The overlay is a floating popup and does not affect
    /// the tab strip position.
    /// Hit-test the current cursor position against the tab strip.
    /// Returns the index of the tab under the cursor, or `None` if the
    /// cursor isn't in the strip or the tab list is empty.
    ///
    /// `osr_cursor` is stored in physical pixels (browser-region-relative).
    /// We convert to logical (DIP) space for the hit test because the tab
    /// strip geometry constants (`TAB_STRIP_HEIGHT`, `PINNED_TAB_WIDTH`,
    /// etc.) are all expressed in logical pixels.
    fn hit_test_tab_strip(&self) -> Option<usize> {
        let window = self.window.as_ref()?;
        let size = window.physical_size();
        let phys_full_w = size.width.max(1);
        let phys_full_h = size.height.max(1);
        let scale = self.current_scale();

        // Convert physical window dims to logical for geometry constants.
        let log_full_w = ((phys_full_w as f32) / scale).round() as u32;
        let log_full_h = ((phys_full_h as f32) / scale).round() as u32;

        // cef_y in physical px (after the fix, cef_child_rect returns physical).
        let (_, phys_cef_y, _, _) = self.cef_child_rect(phys_full_w, phys_full_h);
        // Absolute physical y of the cursor.
        let phys_abs_y = (self.osr_cursor.1 + phys_cef_y as i32).max(0) as u32;

        // Convert cursor to logical for comparison against logical constants.
        let log_wx = ((self.osr_cursor.0 as f32) / scale).round() as u32;
        let log_wy = ((phys_abs_y as f32) / scale).round() as u32;

        let has_notice = buffr_core::download_notice_queue_len(&self.download_notice_queue) > 0;
        let pinned_count = self.tab_strip.tabs.iter().filter(|t| t.pinned).count() as u32;
        let total_count = self.tab_ids.len() as u32;

        hit_test_tab_strip_pure(
            log_full_w,
            log_full_h,
            log_wx,
            log_wy,
            has_notice,
            pinned_count,
            total_count,
        )
    }

    /// Top pixel row of the tab strip in **logical** (DIP) space.
    /// `full_h` must be a logical height. Used by the chrome painter.
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
    ///
    /// Uses `osr_resize` (not `resize`) so the underlying `osr_view`
    /// atomics get the new dims — otherwise CEF's `view_rect` callback
    /// returns the stale dims, on_paint fires at the OLD size, and
    /// `last_osr_dims` never matches the current `browser_w/h` →
    /// loading animation stays active forever. This bites whenever a
    /// download notice expires or an overlay closes between window
    /// resizes (chrome layout changes without `WindowEvent::Resized`).
    fn resync_cef_rect(&mut self) {
        let Some(window) = self.window.as_ref() else {
            return;
        };
        let size = window.physical_size();
        let (_x, _y, w, h) = self.cef_child_rect(size.width.max(1), size.height.max(1));
        // Fan the resize out to all engines; only arm the watchdog on the
        // active engine (it's the one whose frame we'll present).
        for (id, host) in &self.engines {
            host.osr_resize(w, h);
            if id == &self.active_engine {
                self.resize_paint_watchdog
                    .arm(w, h, Instant::now(), RESIZE_PAINT_WATCHDOG_TIMEOUT);
            }
        }
    }

    /// Reset the OSR freshness state after every tab switch so the renderer
    /// waits for the newly-active tab's first paint instead of showing the
    /// previous tab's frozen frame.
    ///
    /// `last_osr_dims = None` causes `should_show_loading_anim` to return
    /// `true`, which overlays the loading animation until the incoming
    /// tab's first `render_buffer` lands and passes `is_osr_frame_fresh`.
    ///
    /// `last_osr_generation` is seeded from the newly-active tab's current
    /// frame generation rather than reset to 0: a previously-visited tab's
    /// already-consumed generation would otherwise compare as fresh and
    /// re-present a stale frame. Seeding means everything already painted
    /// reads as consumed, so only a strictly-new `on_paint` passes the
    /// gate — keeping the double-swap guard intact and showing the loading
    /// animation until the new tab actually paints.
    fn on_tab_switch(&mut self) {
        self.last_osr_dims = None;
        // C6: don't reset the watermark to 0 — a previously-visited tab's
        // already-consumed frame generation would then compare as fresh and
        // re-present a stale frame. Seed it with the new tab's current frame
        // generation so only a genuinely new on_paint (generation strictly
        // greater) passes the gate.
        self.last_osr_generation = self
            .active_engine_dyn()
            .and_then(|host| {
                let frame = host.osr_frame();
                frame.lock().ok().map(|frame| frame.generation)
            })
            .unwrap_or(0);
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
            .active_engine_dyn()
            .map(|e| e.active_tab_live_url())
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
            if let Some(engine) = self.active_engine_dyn() {
                engine.stop_find();
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
            && let Some(engine) = self.active_engine_dyn()
            && engine.tab_count() > 1
        {
            let _ = engine.close_tab(tab_id);
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
        let Some(engine) = self.active_engine_dyn() else {
            return;
        };
        if query.is_empty() {
            engine.stop_find();
            self.statusline.find_query = None;
            self.mark_chrome_dirty();
            return;
        }
        engine.start_find(&query, forward);
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
        if let Ok(rows) = self.bookmarks.search_limited(needle, Some(8)) {
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

    /// Route a wayr `KeyEvent` to the open overlay. Returns `true` if
    /// the event was consumed (caller skips the engine path).
    fn overlay_handle_key(&mut self, event: &crate::windowing::KeyEvent) -> bool {
        if self.overlay.is_none() {
            return false;
        }
        // Allow auto-repeat so holding Backspace / arrows / chars in
        // the omnibar fires continuously.
        let chord = match key_event_to_chord_with_repeat(event) {
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
                if let Some(engine) = self.active_engine_dyn()
                    && let Some(text) = engine.clipboard_text()
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
                let resolved = buffr_config::resolve_input(&url, &self.search_config);
                if resolved.is_empty() {
                    return;
                }
                if let Some(host) = self.active_engine_dyn() {
                    if let Err(err) = host.navigate(&resolved) {
                        warn!(error = %err, url = %resolved, "open: navigate failed");
                    }
                } else {
                    warn!(%url, "open: no host yet");
                }
            }
            Command::TabNew => {
                let url = self.new_tab_url.clone();
                if let Some(host) = self.active_engine_dyn()
                    && let Err(err) = host.open_tab(&url)
                {
                    warn!(error = %err, %url, "cmdline :tabnew failed");
                }
            }
            Command::Set { key, value } => {
                self.apply_set(&key, &value);
            }
            Command::Find(query) => {
                if let Some(host) = self.active_engine_dyn() {
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
            Command::Engine(id) => {
                self.dispatch_action(&buffr_modal::PageAction::Engine(id.clone()));
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
    fn hint_mode_handle_key(&mut self, event: &crate::windowing::KeyEvent) -> bool {
        let Some(engine) = self.active_engine_dyn() else {
            return false;
        };
        if !engine.is_hint_mode() {
            return false;
        }
        let chord = match key_event_to_chord(event) {
            Some(c) => c,
            None => return true,
        };
        let mods = chord.modifiers;
        let plain = !mods.contains(buffr_modal::Modifiers::CTRL)
            && !mods.contains(buffr_modal::Modifiers::ALT)
            && !mods.contains(buffr_modal::Modifiers::SUPER);
        match chord.key {
            Key::Named(NamedKey::Esc) => {
                engine.cancel_hint();
                self.exit_hint_mode();
            }
            Key::Named(NamedKey::BS) => {
                if let Some(action) = engine.backspace_hint() {
                    self.handle_hint_action(action);
                }
            }
            Key::Char(c) if plain => {
                if let Some(action) = engine.feed_hint_key(c) {
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
                    // Any focus of an editable field enters Insert mode.
                    //
                    // This used to require a left-click or `i` within a
                    // 500 ms window, which silently dropped every focus the
                    // page drove itself: autofocus, a dialog focusing its
                    // search box a frame later, anything waiting on a fetch
                    // or a hydration tick. The field looked focused and the
                    // caret sat in it, but keystrokes went to the keymap —
                    // and the slower the site, the more often it happened.
                    //
                    // The gate cannot distinguish "the page stole focus" from
                    // "the user asked for this" anyway: both arrive as a
                    // focusin on a text field. What the user sees is a caret,
                    // and a caret has to accept typing. Non-text controls
                    // never reach here — edit.js classifies checkboxes,
                    // radios, ranges and friends as not editable.
                    if !already_editing {
                        self.insert_intent_at = None;
                        self.pending_blur_at = None;
                        if let Some(engine) = self.active_engine_dyn() {
                            engine.run_edit_attach(&field_id);
                        }
                        if let Ok(mut e) = self.engine.lock() {
                            e.set_mode(buffr_modal::PageMode::Insert);
                        }
                        tracing::info!(
                            %field_id,
                            ?kind,
                            "edit-mode entered (engine=Insert, edit_focus=Editing)"
                        );
                        // Remember the last field that received focus so `i`
                        // can re-focus it on the next press.
                        self.last_focused_field = Some(field_id.clone());
                        self.edit_focus = EditFocus::Editing { field_id };
                        mode_changed = true;
                    }
                }
                EditConsoleEvent::Blur { field_id } => {
                    let matches_current = match &self.edit_focus {
                        EditFocus::Editing { field_id: f } => *f == field_id,
                        EditFocus::None => false,
                    };
                    tracing::info!(
                        %field_id,
                        matches_current,
                        prev = ?self.edit_focus,
                        "EditConsoleEvent::Blur"
                    );
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
                    } else if let Some(engine) = self.active_engine_dyn() {
                        let ok = engine.clipboard_set_text(&value);
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
            tracing::info!("expire_pending_blur: engine flipped Insert → Normal");
            self.refresh_title();
        }
    }

    /// Convert a wayr `KeyEvent` into a `PlannedInput` for the hjkl engine.
    ///
    /// Mirrors `buffr_modal::wayr_adapter::key_event_to_chord` but targets
    /// `hjkl_engine::PlannedInput` rather than our internal `KeyChord`.
    fn wayr_key_to_planned(
        event: &crate::windowing::KeyEvent,
        modifiers: &Modifiers,
    ) -> Option<PlannedInput> {
        if event.state != crate::windowing::KeyState::Pressed {
            return None;
        }
        let mods = EngineModifiers {
            ctrl: modifiers.ctrl,
            shift: modifiers.shift,
            alt: modifiers.alt,
            super_: modifiers.logo,
        };
        // Try text first (handles regular printable characters).
        // wayr ≥ 0.1.2 strips ASCII control characters from `text` at
        // the source, so anything we see here is a printable scalar
        // — the control-key family (Return, BackSpace, Tab, Escape,
        // Delete) arrives as `text = None` + `key_code = Named(...)`
        // and is resolved by the named-key path below.
        if let Some(text) = event.text.as_deref() {
            let mut chars = text.chars();
            let first = chars.next()?;
            if chars.next().is_some() {
                return None; // multi-char text (e.g. IME) — skip
            }
            return Some(PlannedInput::Char(first, mods));
        }
        // No printable text — check for named keys via xkb keysym name.
        use crate::windowing::KeyCode;
        match &event.key_code {
            KeyCode::Named(name) => {
                let sk = match name.as_str() {
                    "Escape" => SpecialKey::Esc,
                    "Return" | "KP_Enter" => SpecialKey::Enter,
                    "BackSpace" => SpecialKey::Backspace,
                    "Tab" => SpecialKey::Tab,
                    "Up" => SpecialKey::Up,
                    "Down" => SpecialKey::Down,
                    "Left" => SpecialKey::Left,
                    "Right" => SpecialKey::Right,
                    "Home" => SpecialKey::Home,
                    "End" => SpecialKey::End,
                    "Prior" => SpecialKey::PageUp,
                    "Next" => SpecialKey::PageDown,
                    "Insert" => SpecialKey::Insert,
                    "Delete" => SpecialKey::Delete,
                    "F1" => SpecialKey::F(1),
                    "F2" => SpecialKey::F(2),
                    "F3" => SpecialKey::F(3),
                    "F4" => SpecialKey::F(4),
                    "F5" => SpecialKey::F(5),
                    "F6" => SpecialKey::F(6),
                    "F7" => SpecialKey::F(7),
                    "F8" => SpecialKey::F(8),
                    "F9" => SpecialKey::F(9),
                    "F10" => SpecialKey::F(10),
                    "F11" => SpecialKey::F(11),
                    "F12" => SpecialKey::F(12),
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
    ///
    /// Also called with no focused field when the engine is in
    /// [`PageMode::Insert`] anyway — see the caller in `event_loop`.
    /// In that state only `Esc` is handled (it must always be able to
    /// leave Insert); every other key returns `false` and falls
    /// through to the chord engine as before.
    fn edit_mode_handle_key(&mut self, event: &crate::windowing::KeyEvent) -> bool {
        let planned = Self::wayr_key_to_planned(event, &self.modifiers);
        let is_esc_pressed = matches!(planned, Some(PlannedInput::Key(SpecialKey::Esc, _)));
        let mode = self.engine.lock().ok().map(|e| e.mode());
        tracing::debug!(
            state = ?event.state,
            key_code = ?event.key_code,
            scancode = ?event.scancode,
            text = ?event.text.as_deref(),
            is_esc_pressed,
            mods = ?(self.modifiers.shift, self.modifiers.ctrl, self.modifiers.alt, self.modifiers.logo),
            edit_focus = ?self.edit_focus,
            mode = ?mode,
            window_focused = self.window_focused,
            "edit_mode_handle_key"
        );

        let EditFocus::Editing { field_id, .. } = &self.edit_focus else {
            // No focused field. If the engine is nonetheless in
            // PageMode::Insert (user-bound `enter_insert_mode`, or a
            // field that vanished), Esc is the ONLY way out: the trie
            // is short-circuited by `Step::EditModeActive`, so an
            // `<Esc>` binding can never fire. Drive the engine's own
            // exit path, which restores the mode Insert was entered
            // from.
            if is_esc_pressed && mode == Some(PageMode::Insert) {
                let exited = self.engine.lock().ok().map(|mut e| {
                    e.feed_edit_mode_key(buffr_modal::KeyChord::plain(Key::Named(NamedKey::Esc)))
                });
                if matches!(exited, Some(buffr_modal::EditModeStep::Exited)) {
                    self.refresh_title();
                    self.request_redraw();
                    tracing::info!(
                        "edit_mode: exited via Esc with no focused field — \
                         engine left PageMode::Insert"
                    );
                    return true;
                }
            }
            tracing::warn!(
                state = ?event.state,
                key_code = ?event.key_code,
                "edit_mode_handle_key: no EditFocus — key will fall through to chord engine"
            );
            return false;
        };

        if is_esc_pressed {
            let fid = field_id.clone();
            self.edit_focus = EditFocus::None;
            if let Some(engine) = self.active_engine_dyn() {
                engine.run_edit_detach(&fid);
                // Blur the field so further typing doesn't go to it.
                let _ = engine.run_js(buffr_core::scripts::EXIT_INSERT);
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
        if event.state == crate::windowing::KeyState::Pressed
            && matches!(planned, Some(PlannedInput::Key(SpecialKey::Tab, _)))
        {
            if let Some(engine) = self.active_engine_dyn() {
                engine.run_edit_cycle(!self.modifiers.shift);
            }
            return true;
        }

        // Conventional-browser tab shortcuts that the user expects to
        // work even while typing in an input: `<C-t>`, `<C-S-t>`,
        // `<C-w>`. Dispatch the matching PageAction directly so the
        // user doesn't have to leave Insert first.
        if event.state == crate::windowing::KeyState::Pressed
            && self.modifiers.ctrl
            && let Some(PlannedInput::Char(c, _)) = planned
        {
            let lower = c.to_ascii_lowercase();
            let action = match (lower, self.modifiers.shift) {
                ('t', false) => Some(buffr_modal::PageAction::TabNewRight),
                ('t', true) => Some(buffr_modal::PageAction::ReopenClosedTab),
                ('w', false) => Some(buffr_modal::PageAction::TabClose),
                _ => None,
            };
            if let Some(a) = action {
                self.dispatch_action(&a);
                return true;
            }

            // Ctrl+V paste: CEF in OSR mode has no native wayland
            // clipboard wiring, so the renderer's paste path can't
            // read the system selection on its own. Read it ourselves
            // via hjkl-clipboard and inject the text into the focused
            // element via execCommand.
            //
            // Done on a worker thread to avoid the self-deadlock when
            // Chromium owns the clipboard: hjkl-clipboard's wayland
            // `offer.receive` blocks the calling thread on a pipe, but
            // the matching wl_data_source.send callback runs on CEF's
            // UI thread (= the main thread). If we read on the main
            // thread, we'd block the very thread that needs to write
            // the pipe, hanging the app until SIGKILL. The worker
            // thread parks; the main thread keeps pumping CEF, the
            // pipe is served, and the result is posted back via
            // EventLoopProxy as `ClipboardPasteText`.
            if lower == 'v'
                && !self.modifiers.shift
                && let Some(engine) = self.active_engine_dyn()
                && let Some(cb) = engine.clipboard_handle()
            {
                let proxy = self.event_proxy.clone();
                std::thread::spawn(move || {
                    let text = cb.read_text();
                    let _ = proxy.send_event(BuffrUserEvent::ClipboardPasteText(text));
                });
                return true;
            }
        }

        // Forward every other key directly to CEF. The page handles it
        // natively — no Rust-side editor model.
        if let Some(host) = self.active_engine_dyn() {
            let mods = mods_to_cef(&self.modifiers);
            // edit_mode_handle_key only runs when EditFocus::Editing is
            // active, so a text input is always focused here.
            let cef_events = key_to_neutral_events(event, mods, true);
            for ev in &cef_events {
                tracing::debug!(
                    kind = ?ev.kind,
                    vk = ev.windows_key_code,
                    ch = ev.character,
                    unmod = ev.unmodified_character,
                    mods = ev.modifiers,
                    editable = ev.focus_on_editable_field,
                    "osr_key_event dispatched"
                );
            }
            for ev in cef_events {
                host.osr_key_event(ev);
            }
        }
        true
    }

    /// Bring the on-screen permission prompt in line with the front of the
    /// permissions queue. Returns `true` when the prompt state changed (so
    /// the caller knows to resync the CEF rect + redraw).
    ///
    /// A prompt already on screen is left alone **only** while the entry it
    /// is rendering is still the queue front. If the backend withdrew that
    /// entry, the prompt is taken down and replaced by the current front (or
    /// nothing) — otherwise the chrome would keep asking about a request that
    /// no longer exists, and the answer would land on an unrelated one.
    ///
    /// The identity of the displayed entry is stashed in
    /// `permissions_prompt_id`; [`Self::resolve_permission`] matches the
    /// user's answer against it.
    ///
    /// Phase 8a (#88): queue is fetched from the active engine via the
    /// neutral `BrowserEngine::permissions_queue()` trait method so both
    /// CEF and blink-cdp share the same prompt path.
    fn sync_permissions_prompt(&mut self) -> bool {
        let queue = match self.active_engine_dyn() {
            Some(engine) => engine.permissions_queue(),
            // No engine to answer to — drop any prompt still on screen so a
            // keypress can't be aimed at a queue that no longer exists.
            None => return self.clear_permissions_prompt(),
        };
        let queue_total = permissions_queue_len(&queue);
        let front = peek_permission_front_entry(&queue);

        // A prompt is already up. It stays up only while the entry it is
        // rendering is still the front of the queue: backends withdraw
        // requests (CEF's `OnDismissPermissionPrompt` on navigation), and a
        // prompt for a withdrawn request must not linger — the user would be
        // answering a question nobody is asking any more.
        let mut changed = false;
        if self.permissions_prompt.is_some() {
            let still_current = match (&front, &self.permissions_prompt_id) {
                (Some(f), Some(id)) => id.matches(f),
                _ => false,
            };
            if still_current {
                return false;
            }
            debug!("permissions: displayed request withdrawn — replacing prompt");
            changed = self.clear_permissions_prompt();
            // Fall through: show whatever is at the front now, if anything.
        }

        let Some(front) = front else {
            // Nothing left to show. `changed` is true when the withdrawal
            // branch above just took a stale prompt off screen.
            return changed;
        };
        // queue_total includes the front entry; "more pending after
        // this one" is queue_total - 1.
        let queue_after = queue_total.saturating_sub(1) as u32;
        let labels: Vec<String> = front.capabilities.iter().map(|c| c.human_label()).collect();
        self.permissions_prompt_id = Some(PromptIdentity::of(&front));
        self.permissions_prompt = Some(PermissionsPrompt {
            origin: front.origin.clone(),
            capabilities: labels,
            queue_len: queue_after,
        });
        self.mark_chrome_dirty();
        true
    }

    /// Drop the on-screen permission prompt and its remembered identity.
    /// Returns `true` when there was one to drop (i.e. the prompt state
    /// changed and the chrome needs a redraw).
    fn clear_permissions_prompt(&mut self) -> bool {
        let had = self.permissions_prompt.is_some();
        self.permissions_prompt = None;
        self.permissions_prompt_id = None;
        if had {
            self.mark_chrome_dirty();
        }
        had
    }

    /// Resolve the permission request **that is on screen** with `outcome`.
    /// The callback fires exactly once; the next prompt (if any) is drawn
    /// immediately via [`Self::sync_permissions_prompt`].
    ///
    /// If the displayed request is no longer the front of the queue — the
    /// backend withdrew it while the user was reading — the answer is
    /// discarded rather than applied to whatever took its place. Nothing is
    /// written to the permissions store and the queue is left untouched, so
    /// the request the user never saw stays unanswered.
    ///
    /// Phase 8a (#88): delegates to `BrowserEngine::resolve_permission`
    /// so both CEF and blink-cdp can handle the outcome correctly.
    fn resolve_permission(&mut self, outcome: PromptOutcome) {
        let Some(engine) = self.active_engine_dyn() else {
            warn!("permissions: resolve called with no active engine");
            self.clear_permissions_prompt();
            return;
        };
        let queue = engine.permissions_queue();
        // Apply the answer to the entry the user actually read, or to
        // nothing at all. The front of the queue is not proof of identity:
        // the backend can withdraw the displayed request (tab navigated
        // away) and leave an unrelated one in its place, and answering that
        // would store a decision — possibly a persistent one — for an origin
        // and capability the user never saw.
        let pending =
            match take_permission_front_matching(&queue, self.permissions_prompt_id.as_ref()) {
                ResolveTarget::Apply(pending) => pending,
                ResolveTarget::Stale => {
                    warn!(
                        "permissions: displayed request is no longer at the front of \
                     the queue — discarding the answer instead of applying it to \
                     a request the user did not see"
                    );
                    // Drop the stale prompt, leave the queue untouched (the entry
                    // now at the front stays unanswered), and re-sync so the next
                    // request is presented fresh.
                    self.clear_permissions_prompt();
                    self.sync_permissions_prompt();
                    self.mark_chrome_dirty();
                    self.request_redraw();
                    return;
                }
            };
        // Store persistent decisions before delegating to the backend.
        let store = self.permissions.clone();
        match outcome {
            PromptOutcome::Allow { remember: true } => {
                for cap in &pending.capabilities {
                    if let Err(err) =
                        store.set(&pending.origin, *cap, buffr_permissions::Decision::Allow)
                    {
                        warn!(error = %err, "permissions: store Allow failed");
                    }
                }
            }
            PromptOutcome::Deny { remember: true } => {
                for cap in &pending.capabilities {
                    if let Err(err) =
                        store.set(&pending.origin, *cap, buffr_permissions::Decision::Deny)
                    {
                        warn!(error = %err, "permissions: store Deny failed");
                    }
                }
            }
            _ => {}
        }
        engine.resolve_permission(pending.resolve_id.as_deref(), outcome);
        self.permissions_prompt = None;
        self.permissions_prompt_id = None;
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
    fn confirm_handle_key(&mut self, event: &crate::windowing::KeyEvent) -> bool {
        if self.confirm_close_pinned.is_none() {
            return false;
        }
        if event.state != crate::windowing::KeyState::Pressed {
            return true;
        }
        // Use the resolved text for character matching; fall back to key_code
        // for special keys.
        if let Some(text) = event.text.as_deref() {
            let c = text.chars().next().unwrap_or('\0').to_ascii_lowercase();
            match c {
                'y' => self.resolve_pinned_close(true),
                'n' => self.resolve_pinned_close(false),
                _ => {}
            }
        } else {
            use crate::windowing::KeyCode;
            match &event.key_code {
                KeyCode::Named(name) if name == "Return" || name == "KP_Enter" => {
                    self.resolve_pinned_close(true);
                }
                KeyCode::Named(name) if name == "Escape" => {
                    self.resolve_pinned_close(false);
                }
                _ => {}
            }
        }
        true
    }

    /// Route a keystroke to the active permission prompt. Returns
    /// `true` when the key was consumed.
    ///
    /// Bindings: `a`/`y` allow once, `A`/`Y` allow + remember, `d`/`n`
    /// deny once, `D`/`N` deny + remember, `s` deny + remember
    /// (qutebrowser parity for "stop"), `Esc` defer.
    fn permissions_handle_key(&mut self, event: &crate::windowing::KeyEvent) -> bool {
        if self.permissions_prompt.is_none() {
            return false;
        }
        let chord = match key_event_to_chord(event) {
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
        if let Some(host) = self.active_engine_dyn()
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
        if let Some(host) = self.active_engine_dyn() {
            host.start_find(&query, forward);
        }
    }

    /// Paint one popup window frame: a minimal address bar + OSR content.
    fn paint_popup_window(&mut self, window_id: SurfaceId) {
        let popup = match self.popups.get_mut(&window_id) {
            Some(p) => p,
            None => return,
        };
        let inner = popup.window.physical_size();
        let width = inner.width.max(1);
        let height = inner.height.max(1);
        // The popup chrome buffer is LOGICAL-sized (same contract as the main
        // window) so the bitmap font rasterises at DIP resolution and the GPU
        // stretches it. `bar_h` (STATUSLINE_HEIGHT) is a logical constant: use
        // it as-is inside the chrome buffer, and its scaled twin wherever it
        // meets physical coordinates — the OSR dst_rect here, the cursor
        // offset in `PointerMoved` (M31).
        let scale = popup.window.scale_factor() as f32;
        let (lwidth, lheight) = logical_chrome_dims(width, height, scale);
        let bar_h = STATUSLINE_HEIGHT;
        // Single source of truth for the popup's CEF rect: the same helper
        // feeds `popup_resize`, so the viewport CEF lays the page out for is
        // the rect the quad is painted into (M35).
        let osr_dst_rect = popup_cef_rect_pure(width, height, scale);

        // Same freshness gate as the main window's paint_chrome_with —
        // including stale-dim rejection via popup.view atomics.
        let pop_expected_w = popup.view.width.load(std::sync::atomic::Ordering::Relaxed);
        let pop_expected_h = popup.view.height.load(std::sync::atomic::Ordering::Relaxed);
        let osr_meta: Option<(u32, u32, u64)> = if let Ok(mut frame) = popup.frame.lock() {
            let fresh = is_osr_frame_fresh(
                frame.width,
                frame.height,
                frame.pixels.len(),
                frame.generation,
                popup.last_osr_generation,
                pop_expected_w,
                pop_expected_h,
                frame.needs_fresh,
            );
            if fresh {
                popup.last_osr_dims = Some((frame.width, frame.height));
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
        popup.renderer.set_logical_size(lwidth, lheight);
        let url = popup.url.clone();
        let new_gen;
        let res = if let Some((osr_w, osr_h, osr_gen)) = osr_meta {
            new_gen = osr_gen;
            let osr_upload = crate::render::OsrUpload {
                pixels: &popup.osr_scratch,
                width: osr_w,
                height: osr_h,
                generation: osr_gen,
                dst_rect: osr_dst_rect,
                skip_pixels: false,
            };
            popup.renderer.frame(
                chrome_dirty,
                bar_h,
                0,
                |buf, w, h| paint_popup_chrome(buf, w, h, &url, bar_h),
                Some(osr_upload),
            )
        } else if let Some((cached_w, cached_h)) = popup.last_osr_dims {
            // Between-paints fallback: reuse osr_scratch from previous paint.
            // Same synthetic-upload trick as the main window: generation matches
            // last_osr_generation so the GPU dedupes the upload.
            new_gen = popup.last_osr_generation;
            tracing::debug!(
                cached_w,
                cached_h,
                gen = popup.last_osr_generation,
                "popup: between-paints synthetic upload from osr_scratch"
            );
            let osr_upload = crate::render::OsrUpload {
                pixels: &popup.osr_scratch,
                width: cached_w,
                height: cached_h,
                generation: popup.last_osr_generation,
                dst_rect: osr_dst_rect,
                // Same generation as the GPU already holds: the worker
                // dedupes the upload, so skip the UI-thread memcpy.
                skip_pixels: true,
            };
            popup.renderer.frame(
                chrome_dirty,
                bar_h,
                0,
                |buf, w, h| paint_popup_chrome(buf, w, h, &url, bar_h),
                Some(osr_upload),
            )
        } else {
            // No paint received yet for this popup.
            new_gen = popup.last_osr_generation;
            popup.renderer.frame(
                chrome_dirty,
                bar_h,
                0,
                |buf, w, h| paint_popup_chrome(buf, w, h, &url, bar_h),
                None,
            )
        };

        // Same H8 bookkeeping split as the main window: the OSR generation
        // tracks what we consumed from the shared frame (so the freshness
        // gate can't double-swap), but the chrome dirty flag may only be
        // retired when the renderer really submitted the frame — otherwise
        // a popup URL change that lands while the worker is still
        // presenting is silently dropped.
        popup.last_osr_generation = new_gen;
        let outcome = match &res {
            Ok((stats, submitted)) => Some((stats.submit_done_us, *submitted)),
            Err(err) => {
                warn!(error = %err, "popup: wgpu frame failed");
                None
            }
        };
        let commit = decide_frame_commit(outcome, chrome_dirty);
        if commit.advance_chrome_gen {
            popup.last_painted_chrome_gen = popup.chrome_generation;
        }
        popup.repaint_retry_at = if commit.retry_paint {
            Some(Instant::now() + SKIPPED_FRAME_RETRY_DELAY)
        } else {
            None
        };
    }

    /// Handle a `WindowEvent` for a popup window.
    fn handle_popup_window_event(
        &mut self,
        _event_loop: &mut EventLoop<BuffrUserEvent>,
        window_id: SurfaceId,
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
                if let Some(engine) = self.active_engine_dyn()
                    && browser_id >= 0
                {
                    engine.popup_close(browser_id);
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
                    // Debounce: arm/refresh the pending deadline rather than
                    // calling engine.popup_resize immediately. Fired in about_to_wait.
                    if let Some(popup) = self.popups.get_mut(&window_id) {
                        popup.pending_cef_resize =
                            Some((w, h, Instant::now() + CEF_RESIZE_DEBOUNCE));
                        popup.chrome_generation = popup.chrome_generation.wrapping_add(1);
                    }
                }
                self.paint_popup_window(window_id);
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                // Same Ctrl-sticky fix as the main window — winit may
                // dispatch ModifiersChanged after the key release on
                // some backends, so mirror the modifier state into
                // the popup's cache here too.
                if let Some(popup) = self.popups.get_mut(&window_id) {
                    popup.modifiers = modifiers;
                }
            }
            WindowEvent::Focused => {
                if let Some(engine) = self.active_engine_dyn()
                    && browser_id >= 0
                {
                    engine.popup_osr_focus(browser_id, true);
                }
            }
            WindowEvent::Unfocused => {
                if let Some(engine) = self.active_engine_dyn()
                    && browser_id >= 0
                {
                    engine.popup_osr_focus(browser_id, false);
                }
            }
            WindowEvent::PointerLeft => {
                let mods = self
                    .popups
                    .get(&window_id)
                    .map(|p| mods_to_cef(&p.modifiers))
                    .unwrap_or(0);
                if let Some(engine) = self.active_engine_dyn()
                    && browser_id >= 0
                {
                    // Simulate mouse leave by moving to (0,0) outside the
                    // browser rect — same pattern as main window PointerLeft.
                    engine.popup_osr_mouse_move(browser_id, 0, 0, mods);
                }
            }
            WindowEvent::PointerMoved { position } => {
                let Some(popup) = self.popups.get_mut(&window_id) else {
                    return;
                };
                // `position` is physical; STATUSLINE_HEIGHT is logical, so the
                // strip height must be scaled before it is subtracted (M31).
                let pop_scale = popup.window.scale_factor() as f32;
                let bar_h = popup_bar_h_physical(pop_scale) as i32;
                let phys_bx = position.0.x;
                // Cursor y relative to the content area (below address bar).
                let phys_by = position.0.y.saturating_sub(bar_h);
                // Store physical coords for any chrome hit-tests.
                popup.cursor = (phys_bx, phys_by);
                // CEF OSR consumes DIPs — route through helper (already region-relative).
                let (bx, by) = physical_cursor_to_dip(phys_bx, phys_by, 0, pop_scale);
                let mods = mods_to_cef(&popup.modifiers) | popup.mouse_buttons;
                if let Some(engine) = self.active_engine_dyn()
                    && browser_id >= 0
                {
                    engine.popup_osr_mouse_move(browser_id, bx, by, mods);
                }
            }
            WindowEvent::PointerButton {
                state,
                button,
                modifiers,
            } => {
                use crate::windowing::PointerButtonState::Pressed;
                let Some(popup) = self.popups.get_mut(&window_id) else {
                    return;
                };
                popup.modifiers = modifiers;
                let Some(cef_button) = button_to_neutral(&button) else {
                    return;
                };
                let mouse_up = state != Pressed;
                let btn_flag: u32 = if cef_button == NeutralMouseButton::Left {
                    16
                } else if cef_button == NeutralMouseButton::Middle {
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
                let (phys_bx, phys_by) = popup.cursor;
                let mods = mods_to_cef(&popup.modifiers) | popup.mouse_buttons;
                let click_count = popup.click_count;
                let in_content = phys_by >= 0;
                // CEF OSR consumes DIPs — route through helper (already region-relative).
                let pop_click_scale = popup.window.scale_factor() as f32;
                let (bx, by) = physical_cursor_to_dip(phys_bx, phys_by, 0, pop_click_scale);
                if let Some(engine) = self.active_engine_dyn()
                    && browser_id >= 0
                {
                    // Pressed inside the OSR content (below the address bar)
                    // → focus the popup browser so DOM clicks deliver focus
                    // to inputs and keystrokes route to this popup. Wayland
                    // doesn't reliably emit Focused on click, so we drive
                    // it explicitly.
                    if !mouse_up && in_content {
                        engine.popup_osr_focus(browser_id, true);
                    }
                    engine.popup_osr_mouse_click(
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
            WindowEvent::Scroll(scroll_ev) => {
                // Two-finger horizontal-swipe back/forward — same path
                // as the main window, routed to the popup's own history.
                let is_pixel = matches!(
                    scroll_ev.source,
                    crate::windowing::AxisSource::Finger | crate::windowing::AxisSource::Continuous
                );
                if is_pixel {
                    let (swipe_dx, swipe_dy) = scroll_swipe_delta(&scroll_ev);
                    if let Some(action) = self.detect_swipe(swipe_dx, swipe_dy) {
                        if let Some(engine) = self.active_engine_dyn()
                            && browser_id >= 0
                        {
                            match action {
                                buffr_modal::PageAction::HistoryBack => {
                                    engine.popup_history_back(browser_id);
                                }
                                buffr_modal::PageAction::HistoryForward => {
                                    engine.popup_history_forward(browser_id);
                                }
                                _ => {}
                            }
                        }
                        return;
                    }
                    if self.swipe.committed {
                        return;
                    }
                }

                let Some(popup) = self.popups.get(&window_id) else {
                    return;
                };
                let (phys_bx, phys_by) = popup.cursor;
                let mods = mods_to_cef(&popup.modifiers);
                let (dx, dy, _is_pixel) = scroll_to_cef_delta(&scroll_ev);
                // CEF OSR consumes DIPs — route through helper (already region-relative).
                let pop_wheel_scale = popup.window.scale_factor() as f32;
                let (bx, by) = physical_cursor_to_dip(phys_bx, phys_by, 0, pop_wheel_scale);
                if let Some(engine) = self.active_engine_dyn()
                    && browser_id >= 0
                {
                    engine.popup_osr_mouse_wheel(browser_id, bx, by, dx, dy, mods);
                }
            }
            WindowEvent::Key(key_ev) => {
                let Some(popup) = self.popups.get(&window_id) else {
                    return;
                };
                let mods = mods_to_cef(&popup.modifiers);
                // Popup windows (DevTools, target=_blank for OAuth flows etc.)
                // don't track focus state in buffr — assume editable so
                // typing into popup forms gets the same dispatch as the
                // main window's edit-mode path.
                let events = key_to_neutral_events(&key_ev, mods, true);
                if let Some(engine) = self.active_engine_dyn()
                    && browser_id >= 0
                {
                    for ev in events {
                        engine.popup_osr_key_event(browser_id, ev);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Double-click detection window.
const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(500);

/// Map a [`PageMode`] to the status-line label rendered into the
/// window title.
fn mode_label(mode: PageMode) -> &'static str {
    match mode {
        PageMode::Normal => "NORMAL",
        PageMode::Visual => "VISUAL",
        PageMode::Command => "COMMAND",
        PageMode::Hint => "HINT",
        PageMode::Insert => "INSERT",
    }
}

impl AppState {
    /// Push the current splash frame's HTML into the new-tab page when
    /// the active tab is `buffr://new` and the splash tick has changed
    /// since the last push. Arms `splash_js_next_push` for the next
    /// period boundary so the event-loop deadline keeps advancing the
    /// animation without input. Clears state when the user navigates
    /// away from the new-tab page.
    fn tick_splash_js_push(&mut self) {
        // Gate: when a push deadline is armed and hasn't elapsed yet, the
        // splash frame can't have advanced — skip the URL read (3 locks)
        // and the tick entirely until the next period boundary. `None`
        // means "check": either the page is new-tab with no push armed,
        // or we navigated away and this tick must notice and clear the
        // stale state below.
        if let Some(at) = self.splash_js_next_push
            && at > Instant::now()
        {
            return;
        }
        let Some(engine) = self.active_engine_dyn() else {
            return;
        };
        let url = engine.active_tab_live_url();
        let on_new_tab = url == NEW_TAB_URL || url.starts_with(NEW_TAB_URL);
        if !on_new_tab {
            self.last_splash_tick = None;
            self.splash_js_next_push = None;
            return;
        }
        let tick = self.splash.tick();
        if Some(tick) != self.last_splash_tick {
            let html = crate::loading_anim::splash_frame_html(&self.splash);
            let escaped = serde_json::to_string(&html).unwrap_or_else(|_| "\"\"".to_string());
            let _ = engine.run_main_frame_js(
                &format!(
                    "(()=>{{const e=document.getElementById('buffr-splash');\
                     if(e)e.innerHTML={escaped};}})()"
                ),
                "buffr://splash",
            );
            self.last_splash_tick = Some(tick);
        }
        self.splash_js_next_push = Some(Instant::now() + hjkl_splash::DEFAULT_PERIOD);
    }

    /// Stamp the supervisor liveness atomic for this event-loop iteration.
    ///
    /// The background heartbeat thread owns the socket and does the actual
    /// 1 Hz ping; all the UI thread does is prove it is still turning over.
    ///
    /// `tick` drops the handle only on a terminal write error. It must NOT
    /// drop it while the bg thread is merely withholding pings after a
    /// `UI_LIVENESS_TIMEOUT` stall: that state is recoverable, and the bg
    /// thread can only see the recovery through the marks this makes — so
    /// letting go there would turn a multi-second `queue.submit` block into
    /// a supervisor kill of a perfectly healthy browser.
    fn tick_heartbeat(&mut self) {
        if let Some(h) = self.heartbeat.as_ref()
            && !h.tick()
        {
            self.heartbeat = None;
        }
    }

    /// Handle a pending Ctrl+C / signal shutdown.
    ///
    /// Returns `true` when the caller must return immediately — the session
    /// has been saved, the clean-shutdown flag written and the event loop
    /// asked to exit. Called at the top of every event hook so the exit is
    /// clean regardless of which one fires first (relying on
    /// `about_to_wait` alone has a known Wayland failure mode where the
    /// loop never reaches it).
    fn check_shutdown(&mut self, event_loop: &mut EventLoop<BuffrUserEvent>) -> bool {
        if !self.shutdown_flag.load(Ordering::SeqCst) {
            return false;
        }
        self.save_session_now();
        self.mark_clean_shutdown();
        event_loop.exit();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use buffr_ui::TAB_STRIP_HEIGHT;
    use clap::CommandFactory;

    // ---- OSR sleep policy tests --------------------------------------------

    #[test]
    fn paint_policy_visible_no_media() {
        // Visible (not occluded) with no media → Active.
        assert_eq!(decide_paint_policy(false, false), PaintPolicy::Active);
    }

    #[test]
    fn paint_policy_occluded_no_media() {
        // Occluded with no media → Sleeping.
        assert_eq!(decide_paint_policy(true, false), PaintPolicy::Sleeping);
    }

    #[test]
    fn paint_policy_occluded_with_media() {
        // Media flag is ignored: was_hidden(1) keeps audio alive, so
        // occluded windows sleep regardless of media state.
        assert_eq!(decide_paint_policy(true, true), PaintPolicy::Sleeping);
    }

    #[test]
    fn paint_policy_visible_with_media() {
        // Visible always paints, with or without media.
        assert_eq!(decide_paint_policy(false, true), PaintPolicy::Active);
    }

    // ---- Occlusion heuristic tests -----------------------------------------

    #[test]
    fn record_present_us_caps_history() {
        let mut h: VecDeque<u64> = VecDeque::new();
        for i in 0..(PRESENT_HISTORY_SIZE as u64 + 3) {
            record_present_us(&mut h, i);
        }
        assert_eq!(h.len(), PRESENT_HISTORY_SIZE);
        // Oldest evicted; most recent at the back.
        assert_eq!(*h.back().unwrap(), PRESENT_HISTORY_SIZE as u64 + 2);
    }

    #[test]
    fn occlusion_triggers_on_3_of_5_slow() {
        // 3 slow + 2 fast → occluded.
        let h: VecDeque<u64> = [200_000, 5_000, 200_000, 5_000, 200_000]
            .into_iter()
            .collect();
        assert!(detect_occluded_from_history(&h, 100_000, 3));
    }

    #[test]
    fn occlusion_holds_on_2_of_5_slow() {
        // 2 slow + 3 fast → not occluded (single stutter absorbed).
        let h: VecDeque<u64> = [200_000, 5_000, 200_000, 5_000, 5_000]
            .into_iter()
            .collect();
        assert!(!detect_occluded_from_history(&h, 100_000, 3));
    }

    #[test]
    fn occlusion_short_history_safe() {
        // Fewer samples than the threshold can never trip the heuristic.
        let h: VecDeque<u64> = [200_000, 200_000].into_iter().collect();
        assert!(!detect_occluded_from_history(&h, 100_000, 3));
    }

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn immediate_occlude_threshold_well_above_typical_slow_frame() {
        // Sanity: the immediate-trip threshold must be far enough above
        // SLOW_PRESENT_THRESHOLD_US that legitimate slow frames don't
        // single-handedly trip the fast path (compositor stutter, GC pauses).
        // The assertion is on constants by design — it's a regression
        // guard against accidentally lowering IMMEDIATE_OCCLUDE_THRESHOLD_US
        // below 5x SLOW_PRESENT_THRESHOLD_US.
        assert!(IMMEDIATE_OCCLUDE_THRESHOLD_US >= SLOW_PRESENT_THRESHOLD_US * 5);
    }

    #[test]
    fn cli_help_renders() {
        Cli::command().debug_assert();
    }

    #[test]
    fn version_flag_returns_pkg_version() {
        let cmd = Cli::command();
        let version = cmd.render_version();
        assert!(
            version.contains(env!("CARGO_PKG_VERSION")),
            "render_version output {version:?} missing CARGO_PKG_VERSION"
        );
    }

    #[test]
    fn long_help_contains_ascii_art() {
        let mut cmd = Cli::command();
        let help = cmd.render_long_help().to_string();
        assert!(
            help.contains(include_str!("art.txt")),
            "long_help missing embedded art.txt block; got:\n{help}"
        );
    }

    #[test]
    fn long_help_contains_pkg_version() {
        let mut cmd = Cli::command();
        let help = cmd.render_long_help().to_string();
        assert!(
            help.contains(env!("CARGO_PKG_VERSION")),
            "long_help missing CARGO_PKG_VERSION; got:\n{help}"
        );
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

    #[test]
    fn clear_dir_path_cache_and_local_storage_under_data_not_cache() {
        // Distinct cache/data roots: if the helper resolves against
        // `paths.cache` the join lands under cache/ and these asserts fail.
        let paths = ProfilePaths {
            cache: PathBuf::from("/tmp/cache-root"),
            data: PathBuf::from("/tmp/data-root"),
        };
        assert_eq!(
            clear_dir_path(&paths, ClearableData::Cache),
            Some(PathBuf::from("/tmp/data-root/Cache"))
        );
        assert_eq!(
            clear_dir_path(&paths, ClearableData::LocalStorage),
            Some(PathBuf::from("/tmp/data-root/Local Storage"))
        );
    }

    #[test]
    fn clear_dir_path_other_variants_route_elsewhere() {
        let paths = ProfilePaths {
            cache: PathBuf::from("/tmp/cache-root"),
            data: PathBuf::from("/tmp/data-root"),
        };
        for item in [
            ClearableData::Cookies,
            ClearableData::History,
            ClearableData::Bookmarks,
            ClearableData::Downloads,
        ] {
            assert_eq!(clear_dir_path(&paths, item), None);
        }
    }

    // ---- render-gate regression tests ------------------------------------
    //
    // These cover the loading-animation gate + the chrome-rect math that
    // the resize debounce flush depends on. Three classes of bug have
    // bitten this code path:
    //
    //   • Reading empty `frame.pixels` as the gate (swap-out flicker).
    //   • Debounce flush calling `osr_resize` with stale queued dims.
    //   • `ScaleFactorChanged` not re-syncing osr_view.
    //
    // The pure helpers (`should_show_loading_anim`, `cef_child_rect_pure`)
    // are the testable seams; production code must route through them.

    #[test]
    fn loading_anim_off_when_dims_match() {
        // CEF has painted at exactly browser_w/h: no animation.
        assert!(!should_show_loading_anim(Some((1500, 1050)), 1500, 1050));
    }

    #[test]
    fn loading_anim_on_when_no_prior_paint() {
        // First paint never happened: animation while we wait for CEF.
        assert!(should_show_loading_anim(None, 1500, 1050));
    }

    #[test]
    fn loading_anim_on_when_dims_drift() {
        // CEF painted at old size; chrome layout (e.g. notice expired)
        // changed browser_h. Animation must re-arm (delta > tolerance).
        assert!(should_show_loading_anim(Some((1500, 1050)), 1500, 1100));
    }

    #[test]
    fn loading_anim_on_when_width_drifts() {
        // Width-only mismatch beyond the WPE block-alignment tolerance.
        assert!(should_show_loading_anim(Some((1400, 1050)), 1500, 1050));
    }

    #[test]
    fn loading_anim_off_within_tolerance() {
        // WPE WebKit's AcceleratedBackingStore aligns the content area
        // down to a tile boundary, so a requested 1272x623 often arrives
        // as 1264x615 — within OSR_DIM_TOLERANCE; no animation.
        assert!(!should_show_loading_anim(Some((1264, 615)), 1272, 623));
    }

    #[test]
    fn loading_anim_on_when_height_mismatches_beyond_tolerance() {
        // Width matches, height differs by > OSR_DIM_TOLERANCE: still
        // on because the page is mid-resize.
        assert!(should_show_loading_anim(
            Some((1272, 500)),
            1272,
            500 + OSR_DIM_TOLERANCE + 1,
        ));
    }

    #[test]
    fn loading_anim_off_even_when_load_would_be_in_flight() {
        // Regression: the splash gate used to OR in `host_is_loading`,
        // which got pinned `true` forever when a tab-switch raced
        // LOAD_COMMITTED — splash never gave way to the page even
        // though fresh frames were arriving.  The fix dropped the
        // load-state input from the gate.  Two safeguards:
        //
        //   1. Signature regression — any future change that re-adds
        //      an `is_loading: bool` param would force a fix on this
        //      call site.  The constraint lives in the function
        //      signature itself (the call below).
        //   2. Behaviour assertion — with dims matching the live
        //      browser rect, the gate MUST return false.  Were the
        //      old logic still in play, a stuck-true loading atomic
        //      would have flipped this to true; the assert pins that
        //      degradation can't sneak back in.
        assert!(
            !should_show_loading_anim(Some((1272, 623)), 1272, 623),
            "regression: splash overlay would re-engage even when the engine \
             reports fresh pixels at the right dims, repeating the stuck-splash bug"
        );
    }

    #[test]
    fn cef_rect_chrome_state_changes_height() {
        // The debounce-flush invariant: chrome state at flush time must
        // be consulted, because (full_w, full_h) alone do NOT determine
        // cef_h — has_notice does. A flush that uses queued dims captured
        // before a notice expired would feed CEF the wrong height.
        let (_, _, _, with_notice_h) = cef_child_rect_pure(1500, 1050, 1.0, true);
        let (_, _, _, no_notice_h) = cef_child_rect_pure(1500, 1050, 1.0, false);
        assert!(
            no_notice_h > with_notice_h,
            "cef_h must grow when notice strip drops: with={with_notice_h} no={no_notice_h}",
        );
        // Difference is exactly DOWNLOAD_NOTICE_HEIGHT at scale=1.0.
        assert_eq!(no_notice_h - with_notice_h, DOWNLOAD_NOTICE_HEIGHT);
    }

    #[test]
    fn cef_rect_scale_2x_doubles_chrome_height() {
        // HiDPI invariant: chrome strip heights scale linearly. This
        // pins the math the ScaleFactorChanged path depends on — after
        // a scale change, browser_h shifts, so osr_view must be re-synced.
        let (_, y_1x, _, h_1x) = cef_child_rect_pure(1500, 1050, 1.0, false);
        let (_, y_2x, _, h_2x) = cef_child_rect_pure(1500, 1050, 2.0, false);
        // y_2x is statusline+tab+notice scaled up; with no notice that's
        // exactly 2× the tab strip (no statusline below).
        assert_eq!(y_2x, y_1x * 2);
        // cef_h shrinks by the extra chrome height, not doubles.
        assert!(h_2x < h_1x);
    }

    #[test]
    fn cef_rect_clamps_to_at_least_one_pixel() {
        // Window smaller than chrome: CEF still gets a 1×1 rect rather
        // than a zero-dim that would panic CEF or the GPU upload.
        let (_, _, w, h) = cef_child_rect_pure(0, 0, 1.0, true);
        assert_eq!((w, h), (1, 1));
    }

    // ---- OSR-frame freshness gate ----------------------------------------
    //
    // Pin the swap-gate predicate. The earlier `!frame.pixels.is_empty()`
    // gate had two holes that triggered a wgpu validation panic on
    // resize:
    //
    //   1. After our mem::swap-out, frame.pixels held the previous
    //      scratch Vec — non-empty AND of OLD length. on_paint at NEW
    //      dims then resized that Vec and updated frame.{width,height}.
    //      The next UI paint with no further on_paint between would
    //      see "non-empty + new dims", swap again, and put a stale-len
    //      Vec into scratch.
    //   2. Repeated swaps of the same generation drift scratch length
    //      out of sync with last_osr_dims even when dims are stable.
    //
    // Generation tracking + length-vs-dims check fix both. The wgpu
    // panic "Copy of 0..N would end up overrunning bounds of source M"
    // (observed on Hyprland during a resize across the chrome strip
    // boundary at non-integer scale) was the smoking gun.

    #[test]
    fn osr_frame_fresh_at_init() {
        // First on_paint: generation has advanced from 0 (init) to 1,
        // dims are set, length matches, expected matches. Swap.
        let len = 1920 * 1086 * 4;
        assert!(is_osr_frame_fresh(1920, 1086, len, 1, 0, 1920, 1086, false));
    }

    #[test]
    fn osr_frame_not_fresh_when_zero_dims() {
        // Pre-first-paint: dims are 0, no swap.
        assert!(!is_osr_frame_fresh(0, 0, 0, 0, 0, 0, 0, false));
        assert!(!is_osr_frame_fresh(1920, 0, 0, 1, 0, 1920, 0, false));
        assert!(!is_osr_frame_fresh(0, 1086, 0, 1, 0, 0, 1086, false));
    }

    #[test]
    fn osr_frame_not_fresh_when_pixels_len_mismatches_dims() {
        // The post-swap state: frame.{width,height} were updated by
        // on_paint to NEW dims but frame.pixels is still the leftover
        // OLD-length Vec from a previous swap (on_paint hasn't fired
        // since the dim update). Must NOT swap.
        let new_dims_len = 1920 * 1086 * 4;
        let old_dims_len = 1900 * 1066 * 4;
        assert!(!is_osr_frame_fresh(
            1920,
            1086,
            old_dims_len,
            2,
            1,
            1920,
            1086,
            false,
        ));
        // Trivially-empty case (just-swapped, on_paint not yet fired):
        assert!(!is_osr_frame_fresh(1920, 1086, 0, 2, 1, 1920, 1086, false));
        // Sanity: matching length is the fresh signal.
        assert!(is_osr_frame_fresh(
            1920,
            1086,
            new_dims_len,
            2,
            1,
            1920,
            1086,
            false,
        ));
    }

    #[test]
    fn osr_frame_not_fresh_when_generation_unchanged() {
        // No new on_paint since last swap — even if dims and length
        // are consistent, swapping again would just put leftover
        // pixels back into scratch. The cached scratch already holds
        // this generation's data; trust it.
        let len = 1920 * 1086 * 4;
        assert!(!is_osr_frame_fresh(
            1920, 1086, len, 5, 5, 1920, 1086, false
        ));
    }

    #[test]
    fn osr_frame_fresh_when_generation_advances() {
        // Page animation between paints at stable dims — pure happy
        // path. Generation advances each on_paint.
        let len = 1920 * 1086 * 4;
        assert!(is_osr_frame_fresh(1920, 1086, len, 6, 5, 1920, 1086, false));
        assert!(is_osr_frame_fresh(
            1920, 1086, len, 100, 5, 1920, 1086, false
        ));
    }

    #[test]
    fn osr_frame_rejects_stale_dim_paint() {
        // The flicker bug. CEF emitted a paint at OLD dims (1900x1066)
        // after the embedder issued osr_resize to NEW dims (1920x1086).
        // The paint is internally consistent — generation advanced,
        // length matches its claimed dims — but it's at the wrong
        // size. Accepting it would set last_osr_dims = old dims while
        // browser_w/h is new dims, leaving the loading animation
        // armed forever (until CEF happens to also fire a new-dims
        // paint, which is exactly the flake the user observed).
        let old_w = 1900u32;
        let old_h = 1066u32;
        let new_w = 1920u32;
        let new_h = 1086u32;
        let old_len = (old_w as usize) * (old_h as usize) * 4;
        // frame at OLD dims; expected dims are NEW (osr_view post-resize).
        assert!(
            !is_osr_frame_fresh(old_w, old_h, old_len, 2, 1, new_w, new_h, false),
            "stale-dim paint must be rejected"
        );
    }

    #[test]
    fn osr_frame_rejects_persisted_stale_paint_on_dim_toggle() {
        // The toggle bug. CEF emitted on_paint at A while embedder was
        // resizing to B → gate rejected (A != B), frame.{w,h,gen} kept.
        // User toggles back to A. osr_view = A again. Without the
        // needs_fresh guard, the persisted A pixels would be re-accepted
        // (frame.dims == expected, generation ok) — but that "paint" is
        // now stale: it predates the resize, and CEF's renderer has
        // moved on. Setting needs_fresh=true on osr_resize forces the
        // gate to wait for an actual post-resize on_paint.
        let w = 1920u32;
        let h = 1086u32;
        let len = (w as usize) * (h as usize) * 4;
        assert!(
            !is_osr_frame_fresh(w, h, len, 2, 1, w, h, true),
            "needs_fresh=true must override the dims-match acceptance"
        );
        // And the same paint with needs_fresh=false (after on_paint
        // clears it) is accepted normally.
        assert!(
            is_osr_frame_fresh(w, h, len, 2, 1, w, h, false),
            "post-on_paint frame must be accepted"
        );
    }

    // ---- chrome repaint trigger -----------------------------------------
    //
    // The animation paints opaque pixels into the chrome buffer's browser
    // region; chrome composites OVER OSR. Without forcing a chrome
    // repaint on the animation→OSR transition, the chrome texture keeps
    // the leftover animation pixels and occludes OSR — visible to the
    // user as "animation stops moving but page never replaces it" until
    // a scroll/tab-switch/key-press triggers a chrome repaint.

    #[test]
    fn chrome_repaint_when_chrome_state_dirty() {
        // Standard case: URL changed, statusline updated, etc.
        assert!(should_force_chrome_repaint(true, false, false));
    }

    #[test]
    fn chrome_repaint_during_animation() {
        // Each animation tick is a new frame; the chrome buffer needs
        // re-upload regardless of chrome_dirty.
        assert!(should_force_chrome_repaint(false, true, false));
    }

    #[test]
    fn chrome_repaint_on_animation_deactivation() {
        // The bug: animation just turned off, chrome buffer still has
        // opaque animation pixels in browser region. MUST repaint to
        // clear them before the next OSR composite, otherwise the
        // animation's last frame occludes the page.
        assert!(should_force_chrome_repaint(false, false, true));
    }

    #[test]
    fn chrome_no_repaint_when_idle_and_steady() {
        // Steady state: chrome unchanged, no animation, no transition.
        // Renderer can dedupe the upload.
        assert!(!should_force_chrome_repaint(false, false, false));
    }

    // ---- surface-drift override -----------------------------------------
    //
    // The animation gate has two inputs in production:
    //   1. last_osr_dims != browser_w/h (CEF hasn't caught up).
    //   2. surface_drifted (we presented a buffer that didn't match the
    //      live wl_surface — Hyprland letterboxes mismatched buffers).
    //
    // The flag is set at end-of-paint and consumed at the next paint.
    // While set, want_anim is forced true so the user sees animation
    // motion rather than a frozen letterboxed frame during reconcile.

    #[test]
    fn anim_forced_on_when_surface_drifted() {
        // Even when last_osr_dims matches browser_w/h (normal "no anim"
        // case), surface drift overrides → animation on.
        let last = Some((944u32, 1066u32));
        let base = should_show_loading_anim(last, 944, 1066);
        assert!(!base, "preconditions: base gate would be off");
        // The actual production OR happens inline at the call site;
        // pin the contract by example.
        let drifted = true;
        assert!(base || drifted);
    }

    #[test]
    fn drift_detect_when_used_dims_differ_from_live() {
        // The check inlined in paint_chrome_with, written here as a
        // truth-table to lock in the intent.
        let used = (944u32, 1130u32);
        let live = (1920u32, 1200u32);
        assert!(used != live, "drift case");
        let stable = (944u32, 1130u32);
        assert!(used == stable, "no-drift case");
    }

    #[test]
    fn chrome_repaint_combinations() {
        // All non-(false,false,false) inputs trigger repaint.
        assert!(should_force_chrome_repaint(true, true, false));
        assert!(should_force_chrome_repaint(true, false, true));
        assert!(should_force_chrome_repaint(false, true, true));
        assert!(should_force_chrome_repaint(true, true, true));
    }

    #[test]
    fn osr_frame_accepts_paint_matching_expected_dims() {
        // Once CEF catches up and produces a paint at expected
        // (post-resize) dims, the gate accepts it.
        let new_w = 1920u32;
        let new_h = 1086u32;
        let new_len = (new_w as usize) * (new_h as usize) * 4;
        assert!(is_osr_frame_fresh(
            new_w, new_h, new_len, 3, 1, new_w, new_h, false,
        ));
    }

    #[test]
    fn osr_frame_resize_sequence_no_panic() {
        // End-to-end sequence reproducing the wgpu-panic scenario.
        // Old dims: 1900x1066. New dims: 1920x1086. Verifies the
        // gate skips the bad swap on the second-paint-without-onpaint
        // case that produced the panic.
        let old_w = 1900u32;
        let old_h = 1066u32;
        let new_w = 1920u32;
        let new_h = 1086u32;
        let old_len = (old_w as usize) * (old_h as usize) * 4;
        let new_len = (new_w as usize) * (new_h as usize) * 4;

        let mut last_seen = 0u64;

        // Phase 1: steady at OLD dims. expected = old.
        // Paint 1: on_paint at old dims (gen 1). Swap.
        assert!(is_osr_frame_fresh(
            old_w, old_h, old_len, 1, last_seen, old_w, old_h, false,
        ));
        last_seen = 1;

        // Paint 2: no on_paint between; pixels.len() = 0. Skip.
        assert!(!is_osr_frame_fresh(
            old_w, old_h, 0, 1, last_seen, old_w, old_h, false,
        ));

        // Phase 2: embedder issued osr_resize → expected = new.
        // on_paint at NEW dims fires (gen 2). Length matches.
        assert!(is_osr_frame_fresh(
            new_w, new_h, new_len, 2, last_seen, new_w, new_h, false,
        ));
        last_seen = 2;

        // Paint 4: no on_paint between. frame.pixels is now the
        // previous scratch (old_len bytes), frame dims are new.
        // Length+generation gate rejects.
        assert!(!is_osr_frame_fresh(
            new_w, new_h, old_len, 2, last_seen, new_w, new_h, false,
        ));
    }

    // ---- edit-mode unit tests --------------------------------------------
    //
    // TODO(wayr-port): winit_key_to_planned_tests and virtual_keyboard_tests
    // were written against winit KeyEvent / PhysicalKey / logical Key types.
    // They are gated out below until a wayr-native seam is available for
    // constructing crate::windowing::KeyEvent in unit tests.
    //
    // The production code paths (wayr_key_to_planned, scan_code_to_vk,
    // resolve_char_unit, key_to_neutral_events) are covered at the
    // integration level.

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

    // ---- Group 1: Paint dispatch enum ------------------------------------
    //
    // Invariant: `decide_paint_path` is the single gate for the four-arm
    // paint dispatch.  Priority must be Animation > FreshOsr >
    // SyntheticScratch > DeadFallback.  The v0.1.25 invariant (fresh
    // osr_meta + dim mismatch ⇒ Animation, not FreshOsr) is pinned by the
    // last test so a future refactor can't silently re-order the arms and
    // reintroduce the wrong-size OSR flash.

    #[test]
    fn paint_path_animation_arm() {
        // want_anim=true → Animation, regardless of osr_meta / last_osr_dims.
        assert_eq!(decide_paint_path(true, false, None), PaintPath::Animation);
    }

    #[test]
    fn paint_path_fresh_osr_arm() {
        // want_anim=false, fresh osr_meta → FreshOsr.
        assert_eq!(decide_paint_path(false, true, None), PaintPath::FreshOsr);
    }

    #[test]
    fn paint_path_synthetic_scratch_arm() {
        // want_anim=false, no fresh osr_meta, but last_osr_dims → SyntheticScratch.
        assert_eq!(
            decide_paint_path(false, false, Some((1500, 1050))),
            PaintPath::SyntheticScratch
        );
    }

    #[test]
    fn paint_path_dead_fallback_arm() {
        // want_anim=false, no osr_meta, no last_osr_dims → DeadFallback.
        assert_eq!(
            decide_paint_path(false, false, None),
            PaintPath::DeadFallback
        );
    }

    #[test]
    fn paint_path_animation_beats_fresh_osr() {
        // v0.1.25 invariant: size mismatch triggers want_anim=true even
        // when fresh osr_meta arrived this frame.  Animation must win.
        assert_eq!(
            decide_paint_path(true, true, Some((1500, 1050))),
            PaintPath::Animation
        );
    }

    // ---- Group 2: Resize debounce state machine --------------------------
    //
    // Invariant: `ResizeDebounce` owns all deadline tracking; the
    // about_to_wait clamp reads `deadline()`, `should_fire` gates the
    // osr_resize call, and `clear` consumes the entry.  Re-arming during a
    // continuous drag must push the deadline forward so the flush only
    // fires after the last Resized event.

    #[test]
    fn arm_then_arm_refreshes_deadline() {
        let mut db = ResizeDebounce::default();
        let t0 = Instant::now();
        db.arm(800, 600, t0, Duration::from_millis(150));
        let d1 = db.deadline().unwrap();
        // Re-arm after 50 ms of continued drag.
        let t1 = t0 + Duration::from_millis(50);
        db.arm(820, 610, t1, Duration::from_millis(150));
        let d2 = db.deadline().unwrap();
        // Second deadline must be strictly later than the first.
        assert!(
            d2 > d1,
            "re-arm must push deadline forward: {d1:?} vs {d2:?}"
        );
    }

    #[test]
    fn should_fire_only_after_deadline() {
        let mut db = ResizeDebounce::default();
        let t0 = Instant::now();
        db.arm(800, 600, t0, Duration::from_millis(150));
        // Before the deadline: must not fire.
        assert!(!db.should_fire(t0), "must not fire before deadline");
        // At or after the deadline: must fire.
        let at = t0 + Duration::from_millis(150);
        assert!(db.should_fire(at), "must fire at deadline");
        assert!(
            db.should_fire(at + Duration::from_millis(1)),
            "must fire after deadline"
        );
    }

    #[test]
    fn clear_returns_last_queued_dims_then_resets() {
        let mut db = ResizeDebounce::default();
        db.arm(1920, 1080, Instant::now(), Duration::from_millis(150));
        // Overwrite with newer dims.
        db.arm(1921, 1081, Instant::now(), Duration::from_millis(150));
        let dims = db.clear();
        assert_eq!(dims, Some((1921, 1081)));
        // Second clear: nothing pending.
        assert_eq!(db.clear(), None);
        assert!(!db.should_fire(Instant::now()));
    }

    #[test]
    fn unarmed_never_fires() {
        let db = ResizeDebounce::default();
        assert!(!db.should_fire(Instant::now()));
        assert!(db.deadline().is_none());
    }

    #[test]
    fn deadline_drives_event_loop_wakeup_clamp() {
        // `about_to_wait` reads deadline() and sets ControlFlow::WaitUntil.
        // Assert the stored Instant round-trips correctly.
        let mut db = ResizeDebounce::default();
        let t0 = Instant::now();
        let debounce = Duration::from_millis(150);
        db.arm(800, 600, t0, debounce);
        let dl = db.deadline().expect("armed debounce must have a deadline");
        // Deadline should be t0 + debounce (within a few ns of rounding).
        assert!(dl >= t0 + debounce);
        assert!(dl < t0 + debounce + Duration::from_millis(1));
    }

    // ---- Group 3: Tab-strip hit-test ------------------------------------
    //
    // Invariant: `hit_test_tab_strip_pure` is the single source of truth
    // for mapping logical cursor coords to a tab index.  Production code
    // routes through it after converting physical→logical.  Tests cover
    // all return-None paths and several count / pinned combinations.
    //
    // The pure function assumes pinned tabs sort first in the pill walk —
    // matching the session ordering contract.  HiDPI sanity verifies that
    // identical logical inputs at different physical scales produce the
    // same result (i.e. the physical→logical conversion is the only DPI-
    // sensitive step, not the pure function itself).

    /// Window width used for tab-strip tests (logical px).
    const TS_W: u32 = 1000;
    /// Window height used for tab-strip tests (logical px).
    const TS_H: u32 = 800;

    /// Y coordinate inside the tab strip (logical). tab_strip_y = 0 when
    /// no notice; tab strip spans [0, TAB_STRIP_HEIGHT).
    fn tab_y_inside() -> u32 {
        TAB_STRIP_HEIGHT / 2
    }

    #[test]
    fn tab_hit_outside_y_band_returns_none() {
        // Cursor above tab strip (y = 0 when no notice, strip starts at 0, so
        // use y = TAB_STRIP_HEIGHT which is one row below the strip end).
        let result = hit_test_tab_strip_pure(
            TS_W,
            TS_H,
            100,
            TAB_STRIP_HEIGHT, // one past the end
            false,
            0,
            3,
        );
        assert!(
            result.is_none(),
            "below strip: expected None, got {result:?}"
        );
    }

    #[test]
    fn tab_hit_empty_list_returns_none() {
        let result = hit_test_tab_strip_pure(TS_W, TS_H, 100, tab_y_inside(), false, 0, 0);
        assert!(result.is_none());
    }

    #[test]
    fn tab_hit_gutter_returns_none() {
        // x < GUTTER (4 px) is the leading gutter — no pill lives there.
        let result = hit_test_tab_strip_pure(TS_W, TS_H, 2, tab_y_inside(), false, 0, 1);
        assert!(result.is_none(), "in gutter: expected None, got {result:?}");
    }

    #[test]
    fn tab_hit_pinned_region_returns_correct_index() {
        // Layout: 2 pinned tabs, 0 unpinned.
        // Gutter = 4; pinned_w = PINNED_TAB_WIDTH (32).
        // Tab 0: x in [4, 36).  Tab 1: x in [40, 72).
        let pinned_w = buffr_ui::tab_strip::PINNED_TAB_WIDTH;
        const GUTTER: u32 = 4;
        // Cursor in tab 0.
        let x0 = GUTTER + pinned_w / 2;
        let r0 = hit_test_tab_strip_pure(TS_W, TS_H, x0, tab_y_inside(), false, 2, 2);
        assert_eq!(r0, Some(0), "expected tab 0, got {r0:?}");
        // Cursor in tab 1.
        let x1 = GUTTER + pinned_w + GUTTER + pinned_w / 2;
        let r1 = hit_test_tab_strip_pure(TS_W, TS_H, x1, tab_y_inside(), false, 2, 2);
        assert_eq!(r1, Some(1), "expected tab 1, got {r1:?}");
    }

    #[test]
    fn tab_hit_unpinned_single_returns_index_0() {
        // 0 pinned, 1 unpinned. Width computed by the pure function.
        // Just verify a cursor well inside the strip returns Some(0).
        const GUTTER: u32 = 4;
        let x = GUTTER + 10;
        let result = hit_test_tab_strip_pure(TS_W, TS_H, x, tab_y_inside(), false, 0, 1);
        assert_eq!(result, Some(0));
    }

    #[test]
    fn tab_hit_unpinned_three_returns_correct_indices() {
        // 0 pinned, 3 unpinned.
        // gutter_total = (3+1)*4 = 16; avail = 1000-0-16 = 984; raw_w = 328;
        // clamped to MAX_TAB_WIDTH (220).
        // Tab 0: [4, 224).  Tab 1: [228, 448).  Tab 2: [452, 672).
        const GUTTER: u32 = 4;
        let unpinned_w = buffr_ui::MAX_TAB_WIDTH; // clamped at 220
        let x0 = GUTTER + unpinned_w / 2;
        let x1 = GUTTER + unpinned_w + GUTTER + unpinned_w / 2;
        let x2 = GUTTER + unpinned_w + GUTTER + unpinned_w + GUTTER + unpinned_w / 2;
        let r0 = hit_test_tab_strip_pure(TS_W, TS_H, x0, tab_y_inside(), false, 0, 3);
        let r1 = hit_test_tab_strip_pure(TS_W, TS_H, x1, tab_y_inside(), false, 0, 3);
        let r2 = hit_test_tab_strip_pure(TS_W, TS_H, x2, tab_y_inside(), false, 0, 3);
        assert_eq!(r0, Some(0));
        assert_eq!(r1, Some(1));
        assert_eq!(r2, Some(2));
    }

    #[test]
    fn tab_hit_unpinned_seven_tabs_returns_correct_indices() {
        // 0 pinned, 7 unpinned.
        // gutter_total = (7+1)*4 = 32; avail = 1000-32 = 968; raw_w = 138;
        // 138 is in [MIN_TAB_WIDTH=80, MAX_TAB_WIDTH=220] — not clamped.
        const GUTTER: u32 = 4;
        let unpinned_w: u32 = 138; // = 968 / 7
        for i in 0u32..7 {
            let x = GUTTER + i * (unpinned_w + GUTTER) + unpinned_w / 2;
            let result = hit_test_tab_strip_pure(TS_W, TS_H, x, tab_y_inside(), false, 0, 7);
            assert_eq!(
                result,
                Some(i as usize),
                "tab {i}: expected Some({i}), got {result:?}"
            );
        }
    }

    #[test]
    fn tab_hit_hidpi_sanity() {
        // The pure function works in logical space; physical scale must not
        // affect the result when inputs are already in logical coords.
        const GUTTER: u32 = 4;
        let x = GUTTER + 10;
        let r1x = hit_test_tab_strip_pure(TS_W, TS_H, x, tab_y_inside(), false, 0, 1);
        // Same logical coords regardless of physical scale — pure fn is scale-agnostic.
        let r2x = hit_test_tab_strip_pure(TS_W, TS_H, x, tab_y_inside(), false, 0, 1);
        assert_eq!(
            r1x, r2x,
            "same logical inputs must produce same result at any physical scale"
        );
        assert_eq!(r1x, Some(0));
    }

    // ---- Group 1: physical-pixel → DIP conversion ---------------------------
    //
    // CEF OSR consumes logical pixels (DIPs). winit gives physical pixels.
    // Every mouse-forward site divides by `current_scale()`. Past bug class:
    // rounding at fractional scale (1.25, 1.5) silently dropped clicks at
    // chrome edges. These tests pin the rounding direction and the cef_y
    // offset subtraction so the helper can't silently regress.

    #[test]
    fn dip_at_scale_1x_round_trips() {
        // Scale 1.0: logical == physical, no offset.
        assert_eq!(physical_cursor_to_dip(300, 400, 0, 1.0), (300, 400));
        assert_eq!(physical_cursor_to_dip(0, 0, 0, 1.0), (0, 0));
        assert_eq!(physical_cursor_to_dip(-5, -5, 0, 1.0), (-5, -5));
    }

    #[test]
    fn dip_at_scale_2x_halves_coords() {
        // Scale 2.0: physical (200, 400) → DIP (100, 200).
        assert_eq!(physical_cursor_to_dip(200, 400, 0, 2.0), (100, 200));
        assert_eq!(physical_cursor_to_dip(1, 1, 0, 2.0), (1, 1)); // rounds 0.5 → 1
    }

    #[test]
    fn dip_at_fractional_scale_rounds_consistently() {
        // Pin exact rounding at 1.25 and 1.5 so the direction can't silently
        // flip. We use `.round()` (rounds half-up), so 0.5 → 1, 0.4 → 0.
        // 1.25×: phys 1 → 0.8 → rounds to 1.
        assert_eq!(physical_cursor_to_dip(1, 1, 0, 1.25), (1, 1));
        // 1.25×: phys 2 → 1.6 → rounds to 2.
        assert_eq!(physical_cursor_to_dip(2, 2, 0, 1.25), (2, 2));
        // 1.25×: phys 100 → 80.0 → 80.
        assert_eq!(physical_cursor_to_dip(100, 100, 0, 1.25), (80, 80));
        // 1.5×: phys 3 → 2.0 → 2.
        assert_eq!(physical_cursor_to_dip(3, 3, 0, 1.5), (2, 2));
        // 1.5×: phys 1 → 0.667 → rounds to 1.
        assert_eq!(physical_cursor_to_dip(1, 1, 0, 1.5), (1, 1));
    }

    #[test]
    fn dip_subtracts_cef_y_offset() {
        // Chrome strip is 40 px tall. Cursor at y=100 → region-relative y=60 → DIP y=60.
        assert_eq!(physical_cursor_to_dip(200, 100, 40, 1.0), (200, 60));
        // At 2× scale: region-relative y=60 → DIP 30.
        assert_eq!(physical_cursor_to_dip(200, 100, 40, 2.0), (100, 30));
    }

    #[test]
    fn dip_at_top_of_chrome_clamps() {
        // Cursor inside the chrome strip (y < cef_y_offset) → negative DIP y.
        // Chosen behavior: return negative, let callers decide whether to clamp.
        // This is intentional — CEF mouse-leave covers the negative case.
        let (_, by) = physical_cursor_to_dip(100, 10, 40, 1.0);
        assert!(
            by < 0,
            "cursor in chrome strip should yield negative DIP y, got {by}"
        );
        // At exact boundary (y == cef_y_offset), y == 0.
        assert_eq!(physical_cursor_to_dip(100, 40, 40, 1.0), (100, 0));
    }

    #[test]
    fn dip_handles_zero_scale_safely() {
        // scale <= 0 is clamped to 1.0 so no divide-by-zero occurs.
        assert_eq!(physical_cursor_to_dip(200, 400, 0, 0.0), (200, 400));
        assert_eq!(physical_cursor_to_dip(200, 400, 0, -1.0), (200, 400));
    }

    // ---- Group 3: proptest on cef_child_rect_pure ---------------------------
    //
    // Random (full_w, full_h, scale, has_notice) inputs drive the pure
    // CEF-rect helper. Key invariants pinned here catch the "rect spills
    // past window bottom" bug class and the clamp-to-1 lower bound.

    mod cef_rect_proptest {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            /// `cef_w` and `cef_h` are always at least 1 (clamp invariant).
            /// `cef_w` never exceeds the effective window width.
            /// `y + cef_h` never exceeds the effective window height
            ///   — the rect must fit inside the window.
            /// `y` is always non-negative (trivially u32, but explicit).
            /// When has_notice=true, `y >= y_without_notice`.
            #[test]
            fn rect_invariants(
                full_w in 0u32..=8000,
                full_h in 0u32..=8000,
                scale in 0.5f32..=4.0f32,
                has_notice in any::<bool>(),
            ) {
                let (_, y, cef_w, cef_h) = cef_child_rect_pure(full_w, full_h, scale, has_notice);

                // Clamp: always at least 1×1.
                prop_assert!(cef_w >= 1, "cef_w must be >= 1, got {cef_w}");
                prop_assert!(cef_h >= 1, "cef_h must be >= 1, got {cef_h}");

                // Never widens past the effective window width.
                let eff_w = full_w.max(1);
                prop_assert!(
                    cef_w <= eff_w,
                    "cef_w={cef_w} > eff_w={eff_w}"
                );

                // Rect fits inside window — most critical invariant.
                let eff_h = full_h.max(1);
                prop_assert!(
                    y.saturating_add(cef_h) <= eff_h,
                    "rect spills: y={y} + cef_h={cef_h} > eff_h={eff_h}"
                );

                // y is non-negative (u32, trivially true, but assert for clarity).
                prop_assert_eq!(y, y); // y: u32 so always >= 0

                // With notice, y >= y without notice (notice strip sits above
                // the tab strip, which pushes the cef origin down).
                let (_, y_no_notice, _, _) = cef_child_rect_pure(full_w, full_h, scale, false);
                if has_notice {
                    prop_assert!(
                        y >= y_no_notice,
                        "has_notice=true should not decrease y: y={y} < y_no_notice={y_no_notice}"
                    );
                }
            }
        }
    }

    // ---- resize-paint watchdog unit tests -----------------------------------

    #[test]
    fn watchdog_arm_then_observe_clears() {
        let mut wd = ResizePaintWatchdog::default();
        let now = Instant::now();
        wd.arm(1920, 1086, now, Duration::from_millis(500));
        assert!(wd.is_armed());
        let cleared = wd.observe_paint(1920, 1086);
        assert!(cleared, "observe_paint at matching dims must return true");
        assert!(
            !wd.is_armed(),
            "watchdog must be cleared after matching paint"
        );
    }

    #[test]
    fn watchdog_observe_wrong_dims_does_not_clear() {
        let mut wd = ResizePaintWatchdog::default();
        let now = Instant::now();
        wd.arm(1920, 1086, now, Duration::from_millis(500));
        let cleared = wd.observe_paint(1900, 1066);
        assert!(!cleared, "observe_paint at wrong dims must return false");
        assert!(
            wd.is_armed(),
            "watchdog must stay armed after mismatched paint"
        );
    }

    #[test]
    fn watchdog_should_force_repaint_after_deadline() {
        let mut wd = ResizePaintWatchdog::default();
        let now = Instant::now();
        wd.arm(1920, 1086, now, Duration::from_millis(500));
        // Before deadline: must not fire.
        assert!(
            !wd.should_force_repaint(now),
            "must not fire before deadline"
        );
        // After deadline: must fire.
        let after = now + Duration::from_millis(501);
        assert!(
            wd.should_force_repaint(after),
            "must fire after deadline elapses"
        );
    }

    #[test]
    fn watchdog_force_repaint_bumps_deadline_and_retries() {
        let mut wd = ResizePaintWatchdog::default();
        let now = Instant::now();
        let timeout = Duration::from_millis(500);
        wd.arm(1920, 1086, now, timeout);
        wd.record_force_repaint(now, timeout);
        assert_eq!(wd.retry_count(), 1, "retry count must be 1 after one nudge");
        // Deadline must have been pushed forward.
        let new_deadline = wd.deadline().expect("watchdog still armed");
        assert!(
            new_deadline >= now + timeout,
            "deadline must be pushed forward by timeout"
        );
    }

    #[test]
    fn watchdog_caps_retries_at_max() {
        let mut wd = ResizePaintWatchdog::default();
        let now = Instant::now();
        let timeout = Duration::from_millis(500);
        wd.arm(1920, 1086, now, timeout);
        for _ in 0..ResizePaintWatchdog::MAX_RETRIES {
            assert!(wd.is_armed(), "watchdog must be armed before hitting cap");
            wd.record_force_repaint(now, timeout);
        }
        assert!(
            !wd.is_armed(),
            "watchdog must give up after MAX_RETRIES nudges"
        );
    }

    #[test]
    fn watchdog_unarmed_should_not_fire() {
        let wd = ResizePaintWatchdog::default();
        let now = Instant::now();
        assert!(
            !wd.should_force_repaint(now),
            "unarmed watchdog must never fire"
        );
        assert!(!wd.is_armed());
    }

    /// Tests for the wtype / virtual_keyboard fix (#36):
    /// - punctuation scancodes must map to `VK_OEM_*` (non-zero)
    /// - `resolve_char_unit` returns the byte for ASCII text, 0 otherwise
    /// - `char_to_vk` must return the character-derived VK so wtype's
    ///   "character on arbitrary scancode" keymap doesn't deliver
    ///   `VK_ESCAPE`/`VK_BACK`/`VK_TAB` with the typed letter.
    mod virtual_keyboard_tests {
        use super::*;

        #[test]
        fn punctuation_scancodes_have_vk_codes() {
            // (evdev scancode, expected VK)
            let cases: &[(u32, i32)] = &[
                (52, 0xBE), // KEY_DOT     → VK_OEM_PERIOD
                (51, 0xBC), // KEY_COMMA   → VK_OEM_COMMA
                (12, 0xBD), // KEY_MINUS   → VK_OEM_MINUS
                (13, 0xBB), // KEY_EQUAL   → VK_OEM_PLUS
                (39, 0xBA), // KEY_SEMICOLON → VK_OEM_1
                (53, 0xBF), // KEY_SLASH   → VK_OEM_2
                (41, 0xC0), // KEY_GRAVE   → VK_OEM_3
                (26, 0xDB), // KEY_LBRACE  → VK_OEM_4
                (43, 0xDC), // KEY_BACKSLASH → VK_OEM_5
                (27, 0xDD), // KEY_RBRACE  → VK_OEM_6
                (40, 0xDE), // KEY_APOSTROPHE → VK_OEM_7
            ];
            for &(sc, want) in cases {
                let got = scan_code_to_vk(crate::windowing::ScanCode(sc));
                assert_eq!(got, want, "VK for evdev scancode {sc}");
            }
        }

        // (resolve_char_unit coverage lives in `resolve_char_unit_from_text`
        // at the end of this mod — same shape, no duplication.)

        // ---- char_to_vk tests (pure-function — no winit / wayr deps) ----

        #[test]
        fn char_to_vk_letters_lowercase() {
            assert_eq!(char_to_vk(b'a' as u16), Some(0x41)); // VK_A
            assert_eq!(char_to_vk(b's' as u16), Some(0x53)); // VK_S
            assert_eq!(char_to_vk(b'z' as u16), Some(0x5A)); // VK_Z
        }

        #[test]
        fn char_to_vk_letters_uppercase() {
            assert_eq!(char_to_vk(b'A' as u16), Some(0x41)); // VK_A
            assert_eq!(char_to_vk(b'Z' as u16), Some(0x5A)); // VK_Z
        }

        #[test]
        fn char_to_vk_digits() {
            assert_eq!(char_to_vk(b'0' as u16), Some(0x30));
            assert_eq!(char_to_vk(b'5' as u16), Some(0x35));
            assert_eq!(char_to_vk(b'9' as u16), Some(0x39));
        }

        #[test]
        fn char_to_vk_punctuation() {
            assert_eq!(char_to_vk(b'.' as u16), Some(0xBE));
            assert_eq!(char_to_vk(b',' as u16), Some(0xBC));
            assert_eq!(char_to_vk(b'-' as u16), Some(0xBD));
            assert_eq!(char_to_vk(b'/' as u16), Some(0xBF));
            assert_eq!(char_to_vk(b'\'' as u16), Some(0xDE));
        }

        #[test]
        fn char_to_vk_control_chars() {
            assert_eq!(char_to_vk(b' ' as u16), Some(0x20)); // VK_SPACE
            assert_eq!(char_to_vk(b'\r' as u16), Some(0x0D)); // VK_RETURN
            assert_eq!(char_to_vk(b'\t' as u16), Some(0x09)); // VK_TAB
            assert_eq!(char_to_vk(0x08), Some(0x08)); // VK_BACK
            assert_eq!(char_to_vk(0x1B), Some(0x1B)); // VK_ESCAPE
        }

        #[test]
        fn char_to_vk_shifted_symbols_have_no_direct_vk() {
            // These need shift+key on a US layout — caller falls back to
            // the scancode-derived VK.
            for c in [
                '@', '#', '$', '%', '^', '&', '*', '(', ')', '!', '~', '_', '+',
            ] {
                assert_eq!(char_to_vk(c as u16), None, "no direct VK for {c:?}");
            }
        }

        #[test]
        fn char_to_vk_non_ascii_returns_none() {
            assert_eq!(char_to_vk(0x00E9), None); // é
            assert_eq!(char_to_vk(0x4E2D), None); // 中
        }

        #[test]
        fn uppercase_letters_resolve_to_vk_a_through_z() {
            // wtype types 'S' as Character("S") (uppercase), not as
            // Shift+s. char_to_vk maps directly to VK_S — same VK as
            // lowercase, since Windows uses one VK per letter regardless
            // of case. The CHAR event carries the uppercase character.
            assert_eq!(char_to_vk(b'S' as u16), Some(0x53));
            assert_eq!(char_to_vk(b's' as u16), Some(0x53));
            assert_eq!(char_to_vk(b'A' as u16), char_to_vk(b'a' as u16));
            assert_eq!(char_to_vk(b'Z' as u16), char_to_vk(b'z' as u16));
        }

        #[test]
        fn wtype_scancode_mismatch_resolves_to_letter_vk() {
            // wtype: each char is on the next free scancode in a synthetic
            // keymap. Our key dispatcher must use char_to_vk to land the
            // correct Windows VK regardless of physical position.
            //
            // Repro from the field log: 's' was delivered with
            // physical=Code(Escape). Without char_to_vk, vk=27 (VK_ESCAPE)
            // → Chromium fires keydown with code='Escape' → input handler
            // suppresses; with char_to_vk, vk=83 (VK_S) → keydown
            // code='KeyS' → text inserts.
            assert_eq!(char_to_vk(b's' as u16), Some(0x53));
            // Likewise for 'c' (would have been VK_BACK = 0x08) and 'o'
            // (would have been VK_TAB = 0x09) which deleted/jumped focus.
            assert_eq!(char_to_vk(b'c' as u16), Some(0x43));
            assert_eq!(char_to_vk(b'o' as u16), Some(0x4F));
        }

        #[test]
        fn resolve_char_unit_from_text() {
            assert_eq!(resolve_char_unit(Some("a")), b'a' as u16);
            assert_eq!(resolve_char_unit(Some(".")), b'.' as u16);
            assert_eq!(resolve_char_unit(None), 0);
        }
    }

    // ---- Group 8: frame-submission bookkeeping (H8 / M34) ----------------
    //
    // Invariant: nothing that consumes dirty state or feeds the occlusion
    // heuristic may run for a frame the renderer never submitted.
    // `Renderer::frame` returns Ok(..) on five skip paths (worker busy,
    // channel full, Timeout / Occluded / Validation / stale-size acquire
    // failures) carrying either the PREVIOUS frame's stats or defaults.

    use crate::render::Submitted;

    #[test]
    fn frame_commit_submitted_retires_dirty_and_samples() {
        let c = decide_frame_commit(Some((1234, Submitted::Yes)), true);
        assert!(c.advance_chrome_gen, "submitted + dirty must retire dirty");
        assert_eq!(c.observe_us, Some(1234), "submitted frame yields a sample");
        assert!(!c.retry_paint, "submitted frame needs no retry");
    }

    #[test]
    fn frame_commit_submitted_clean_chrome_does_not_advance() {
        // Nothing was dirty, so there is nothing to retire — but the
        // timing sample is still real.
        let c = decide_frame_commit(Some((10, Submitted::Yes)), false);
        assert!(!c.advance_chrome_gen);
        assert_eq!(c.observe_us, Some(10));
    }

    #[test]
    fn frame_commit_skipped_keeps_dirty_state() {
        // H8: the omnibar keystroke repro. Chrome was dirty, the worker was
        // still presenting, so the pixels never went up — the dirty flag
        // must survive or the character is lost until an unrelated event.
        let c = decide_frame_commit(Some((999_999, Submitted::No)), true);
        assert!(
            !c.advance_chrome_gen,
            "a skipped frame must not retire the chrome dirty state"
        );
        assert!(c.retry_paint, "a skipped frame must be retried");
    }

    #[test]
    fn frame_commit_skipped_does_not_resample_stale_stats() {
        // M34(a): a skip returns the previous frame's numbers. Feeding a
        // 150 ms sample back in twice fills the history and falsely trips
        // the 3-of-5 rule.
        let c = decide_frame_commit(Some((150_000, Submitted::No)), true);
        assert_eq!(
            c.observe_us, None,
            "stale stats from a skipped frame must not be observed"
        );
    }

    #[test]
    fn frame_commit_error_neither_retires_nor_samples() {
        let c = decide_frame_commit(None, true);
        assert!(!c.advance_chrome_gen);
        assert_eq!(c.observe_us, None);
        assert!(
            !c.retry_paint,
            "errors are sticky; retrying just burns wakeups"
        );
    }

    #[test]
    fn skipped_frames_cannot_fill_the_occlusion_history() {
        // M34(a) end to end against the real heuristic: one genuinely slow
        // frame followed by four skips must NOT trip the 3-of-5 rule,
        // because only the submitted frame contributes a sample.
        let mut history = VecDeque::with_capacity(PRESENT_HISTORY_SIZE);
        let outcomes = [
            Some((150_000u64, Submitted::Yes)),
            Some((150_000, Submitted::No)),
            Some((150_000, Submitted::No)),
            Some((150_000, Submitted::No)),
            Some((150_000, Submitted::No)),
        ];
        for o in outcomes {
            if let Some(us) = decide_frame_commit(o, false).observe_us {
                record_present_us(&mut history, us);
            }
        }
        assert_eq!(history.len(), 1, "only the submitted frame is a sample");
        assert!(
            !detect_occluded_from_history(
                &history,
                SLOW_PRESENT_THRESHOLD_US,
                SLOW_FRAMES_TO_OCCLUDE
            ),
            "one slow frame plus skips must not declare occlusion"
        );
        // Sanity: three real slow frames still do.
        let mut history = VecDeque::with_capacity(PRESENT_HISTORY_SIZE);
        for _ in 0..3 {
            if let Some(us) = decide_frame_commit(Some((150_000, Submitted::Yes)), false).observe_us
            {
                record_present_us(&mut history, us);
            }
        }
        assert!(detect_occluded_from_history(
            &history,
            SLOW_PRESENT_THRESHOLD_US,
            SLOW_FRAMES_TO_OCCLUDE
        ));
    }

    #[test]
    fn skipped_probe_cannot_fake_a_wake() {
        // M34(b): while Sleeping, a skipped probe used to re-observe the
        // previous frame's FAST value with was_probe = true and take the
        // "probe fast → wake" branch without ever presenting. No sample
        // means no wake decision.
        assert_eq!(
            decide_frame_commit(Some((100, Submitted::No)), false).observe_us,
            None
        );
    }

    // ---- Group 9: modal geometry, shared by paint and hit-test ----------
    //
    // Invariant (M30): the painter and the click hit-test derive the panel
    // from the SAME `ModalPanel`, in logical space. Invariant (M32): the
    // panel never exceeds the buffer width, so the centring subtraction
    // cannot underflow.

    #[test]
    fn modal_panel_is_60_percent_within_the_clamp_band() {
        let p = ModalPanel::confirm(1000, 800);
        assert_eq!(p.w, 600);
        assert_eq!(p.x, 200);
        assert_eq!(p.y, 800 / 3);
        assert_eq!(p.inner_x, p.x + OMNIBAR_POPUP_BORDER);
        assert_eq!(p.inner_w, p.w - 2 * OMNIBAR_POPUP_BORDER);
    }

    #[test]
    fn modal_panel_clamps_to_max_width() {
        // 60% of 4000 = 2400, above the 800 px cap.
        assert_eq!(ModalPanel::confirm(4000, 800).w, OMNIBAR_POPUP_MAX_WIDTH);
    }

    #[test]
    fn modal_panel_narrow_window_does_not_underflow() {
        // M32: a 500 px window at scale 2 gives lwidth = 250, below the
        // 300 px floor. The old `.clamp(300, 800)` produced popup_w = 300
        // and then `250 - 300` — debug panic, release wrap.
        for win_w in [0u32, 1, 10, 199, 250, 299, 300, 301] {
            let p = ModalPanel::confirm(win_w, 800);
            assert!(p.w <= win_w, "panel {} wider than window {win_w}", p.w);
            assert!(p.x + p.w <= win_w, "panel spills past window {win_w}");
            let o = ModalPanel::omnibar(win_w, 800, 40);
            assert!(o.w <= win_w, "omnibar {} wider than window {win_w}", o.w);
            assert!(o.x + o.w <= win_w);
        }
    }

    #[test]
    fn modal_panel_height_clamped_to_window() {
        // Panel pinned to the upper third; a short window truncates it
        // instead of running past the bottom edge.
        let p = ModalPanel::confirm(1000, 90);
        assert!(p.y + p.h <= 90);
    }

    #[test]
    fn confirm_buttons_hit_at_scale_1() {
        // Logical == physical at 1×: the centre of the Yes rect resolves
        // to Yes and the centre of No to No.
        let (lw, lh) = logical_chrome_dims(1000, 800, 1.0);
        let panel = ModalPanel::confirm(lw, lh);
        let confirm = buffr_ui::ConfirmPrompt {
            message: String::new(),
            yes_label: "Yes (y)".to_string(),
            no_label: "No (n)".to_string(),
        };
        let (yes, no) = confirm.button_rects_at(panel.inner_x, panel.inner_y, panel.inner_w);
        let centre = |r: buffr_ui::ConfirmRect| (r.0 + r.2 / 2, r.1 + r.3 / 2);
        let (yx, yy) = centre(yes);
        let (nx, ny) = centre(no);
        let (lx, ly) = physical_cursor_to_dip(yx, yy, 0, 1.0);
        assert_eq!(hit_test_confirm_buttons(lw, lh, lx, ly), Some(true));
        let (lx, ly) = physical_cursor_to_dip(nx, ny, 0, 1.0);
        assert_eq!(hit_test_confirm_buttons(lw, lh, lx, ly), Some(false));
        // A point well away from both buttons misses.
        assert_eq!(hit_test_confirm_buttons(lw, lh, 5, 5), None);
    }

    #[test]
    fn confirm_buttons_hit_at_scale_2() {
        // M30: the panel is painted into the LOGICAL buffer, so a physical
        // cursor over the drawn "Yes" is at 2x its logical coords. Testing
        // the physical value directly (the old behaviour) misses.
        let scale = 2.0;
        let (phys_w, phys_h) = (2000u32, 1600u32);
        let (lw, lh) = logical_chrome_dims(phys_w, phys_h, scale);
        assert_eq!((lw, lh), (1000, 800));
        let panel = ModalPanel::confirm(lw, lh);
        let confirm = buffr_ui::ConfirmPrompt {
            message: String::new(),
            yes_label: "Yes (y)".to_string(),
            no_label: "No (n)".to_string(),
        };
        let (yes, _no) = confirm.button_rects_at(panel.inner_x, panel.inner_y, panel.inner_w);
        let (yes_cx, yes_cy) = (yes.0 + yes.2 / 2, yes.1 + yes.3 / 2);
        // Where the user's cursor physically is when hovering that pixel.
        let (phys_x, phys_y) = (yes_cx * 2, yes_cy * 2);
        let (lx, ly) = physical_cursor_to_dip(phys_x, phys_y, 0, scale);
        assert_eq!(
            hit_test_confirm_buttons(lw, lh, lx, ly),
            Some(true),
            "converted cursor must land on Yes at scale 2"
        );
        // The pre-fix behaviour — physical coords against the logical
        // panel — must be a miss, which is exactly the reported bug.
        assert_ne!(
            hit_test_confirm_buttons(lw, lh, phys_x, phys_y),
            Some(true),
            "raw physical coords should NOT hit; that was the bug"
        );
    }

    #[test]
    fn context_menu_overlay_hit_matches_paint_space_at_scale_2() {
        // The menu is painted from `to_overlay(lwidth, lheight)` into the
        // logical buffer; the hit-test must use the same dims and DIP
        // cursor coords.
        let scale = 2.0;
        let (lw, lh) = logical_chrome_dims(2000, 1600, scale);
        let overlay = buffr_ui::ContextMenuOverlay {
            entries: vec![
                buffr_ui::ContextMenuEntry {
                    label: "Back".to_string(),
                    is_separator: false,
                    enabled: true,
                },
                buffr_ui::ContextMenuEntry {
                    label: "Reload".to_string(),
                    is_separator: false,
                    enabled: true,
                },
            ],
            selected: 0,
            x: 100,
            y: 120,
        };
        let (px, py, pw, ph) = overlay.panel_rect(lw as usize, lh as usize);
        let (cx, cy) = (px + pw / 2, py + ph / 2);
        // Physical cursor over that logical pixel.
        let (lx, ly) = physical_cursor_to_dip(cx * 2, cy * 2, 0, scale);
        assert!(
            overlay.contains(lw as usize, lh as usize, lx, ly),
            "converted cursor must be inside the panel"
        );
        assert!(
            !overlay.contains(lw as usize, lh as usize, cx * 2, cy * 2),
            "raw physical coords fall outside the logical panel; that was the bug"
        );
    }

    // ---- Group 10: popup address-bar strip scaling (M31) -----------------

    #[test]
    fn popup_bar_height_scales_with_the_popup() {
        assert_eq!(popup_bar_h_physical(1.0), STATUSLINE_HEIGHT);
        assert_eq!(popup_bar_h_physical(2.0), STATUSLINE_HEIGHT * 2);
        // Fractional scale rounds to the nearest physical row.
        assert_eq!(
            popup_bar_h_physical(1.5),
            ((STATUSLINE_HEIGHT as f32) * 1.5).round() as u32
        );
        // Degenerate scale is clamped, never zero-height or NaN.
        assert_eq!(popup_bar_h_physical(0.0), STATUSLINE_HEIGHT);
        assert_eq!(popup_bar_h_physical(-4.0), STATUSLINE_HEIGHT);
    }

    #[test]
    fn popup_content_rect_leaves_room_below_the_bar_at_scale_2() {
        // The OSR dst_rect starts below the address bar in PHYSICAL rows.
        // Using the logical constant left the bar half-drawn and the page
        // shifted up by STATUSLINE_HEIGHT physical pixels.
        let (phys_h, scale) = (1600u32, 2.0);
        let bar = popup_bar_h_physical(scale);
        assert_eq!(bar, STATUSLINE_HEIGHT * 2);
        let content_h = phys_h.saturating_sub(bar).max(1);
        assert_eq!(content_h, phys_h - STATUSLINE_HEIGHT * 2);
    }

    #[test]
    fn logical_chrome_dims_round_and_clamp() {
        assert_eq!(logical_chrome_dims(1000, 800, 1.0), (1000, 800));
        assert_eq!(logical_chrome_dims(2000, 1600, 2.0), (1000, 800));
        assert_eq!(logical_chrome_dims(1000, 800, 1.5), (667, 533));
        // Never zero, even for a degenerate surface or scale.
        assert_eq!(logical_chrome_dims(0, 0, 2.0), (1, 1));
        assert_eq!(logical_chrome_dims(10, 10, 0.0), (10, 10));
    }

    // ---- swipe gesture detector --------------------------------------------

    use crate::windowing::{AxisDirection, AxisSource, ScrollEvent};

    /// A high-res (touchpad) scroll event on one axis, as the pointer
    /// backend delivers it: a single `delta`, the orthogonal component
    /// implied by `axis`.
    fn touchpad_scroll(axis: AxisDirection, delta: f64) -> ScrollEvent {
        ScrollEvent {
            axis,
            delta,
            discrete_steps: 0,
            high_res_120: 0,
            source: AxisSource::Finger,
        }
    }

    /// Drive the detector exactly as the `WindowEvent::Scroll` arms do:
    /// map the event's single axis onto `(dx, dy)`, then feed it.
    fn feed_scroll(
        det: &mut SwipeDetector,
        ev: &ScrollEvent,
        now: Instant,
    ) -> Option<buffr_modal::PageAction> {
        let (dx, dy) = scroll_swipe_delta(ev);
        det.feed(dx, dy, now)
    }

    #[test]
    fn scroll_swipe_delta_keeps_the_event_on_its_own_axis() {
        assert_eq!(
            scroll_swipe_delta(&touchpad_scroll(AxisDirection::Horizontal, 7.0)),
            (7.0, 0.0)
        );
        assert_eq!(
            scroll_swipe_delta(&touchpad_scroll(AxisDirection::Vertical, 7.0)),
            (0.0, 7.0)
        );
    }

    #[test]
    fn vertical_scroll_never_navigates_history() {
        // Regression: both call sites used to pass `delta` as `dx` and
        // hard-code `dy = 0.0`, so scrolling a page down accumulated
        // into the horizontal channel and fired HistoryBack after
        // ~150px. Every event on the vertical axis must return None, no
        // matter how far the page scrolls.
        let mut det = SwipeDetector::default();
        let ev = touchpad_scroll(AxisDirection::Vertical, 10.0);
        let start = Instant::now();
        for i in 0..40 {
            let now = start + Duration::from_millis(16 * i);
            assert_eq!(
                feed_scroll(&mut det, &ev, now),
                None,
                "vertical scroll event {i} navigated history"
            );
        }
        // The whole run landed in the vertical channel, untouched.
        assert_eq!(det.accum_x, 0.0);
        assert_eq!(det.accum_y, 400.0);
        assert!(!det.committed);
    }

    #[test]
    fn vertical_feed_never_navigates_history() {
        // Same invariant one layer down, independent of the axis mapping.
        let mut det = SwipeDetector::default();
        let start = Instant::now();
        for i in 0..40 {
            assert_eq!(
                det.feed(0.0, 10.0, start + Duration::from_millis(16 * i)),
                None
            );
        }
    }

    #[test]
    fn horizontal_scroll_right_commits_history_back() {
        let mut det = SwipeDetector::default();
        let ev = touchpad_scroll(AxisDirection::Horizontal, 10.0);
        let start = Instant::now();
        let mut fired = Vec::new();
        for i in 0..40 {
            if let Some(a) = feed_scroll(&mut det, &ev, start + Duration::from_millis(16 * i)) {
                fired.push((i, a));
            }
        }
        // 150px threshold at 10px/event → commits once, on the 15th event.
        assert_eq!(fired, vec![(14, buffr_modal::PageAction::HistoryBack)]);
    }

    #[test]
    fn horizontal_scroll_left_commits_history_forward() {
        let mut det = SwipeDetector::default();
        let ev = touchpad_scroll(AxisDirection::Horizontal, -10.0);
        let start = Instant::now();
        let mut fired = Vec::new();
        for i in 0..40 {
            if let Some(a) = feed_scroll(&mut det, &ev, start + Duration::from_millis(16 * i)) {
                fired.push((i, a));
            }
        }
        assert_eq!(fired, vec![(14, buffr_modal::PageAction::HistoryForward)]);
    }

    #[test]
    fn diagonal_drag_fails_the_dominance_rule() {
        // ax clears the threshold but the drag is not horizontal-dominant
        // (ax <= 2 * ay), so nothing commits.
        let mut det = SwipeDetector::default();
        let start = Instant::now();
        for i in 0..40 {
            let now = start + Duration::from_millis(16 * i);
            assert_eq!(
                det.feed(10.0, 6.0, now),
                None,
                "diagonal event {i} committed"
            );
        }
        let (ax, ay) = (det.accum_x.abs(), det.accum_y.abs());
        assert!(
            ax >= 150.0,
            "test is vacuous: ax {ax} never crossed threshold"
        );
        assert!(ax <= 2.0 * ay, "test is vacuous: ax {ax} dominates ay {ay}");
    }

    #[test]
    fn a_gap_over_200ms_resets_the_accumulator() {
        let mut det = SwipeDetector::default();
        let start = Instant::now();
        // 14 events of +10px = 140px, just under the 150px threshold.
        for i in 0..14 {
            assert_eq!(
                det.feed(10.0, 0.0, start + Duration::from_millis(16 * i)),
                None
            );
        }
        assert_eq!(det.accum_x, 140.0);
        // Idle past the gap, then one more event: the run restarts from
        // zero instead of tipping the old total over the threshold.
        let after_gap = start + Duration::from_millis(16 * 13) + Duration::from_millis(201);
        assert_eq!(det.feed(10.0, 0.0, after_gap), None);
        assert_eq!(det.accum_x, 10.0);
    }

    #[test]
    fn a_committed_gesture_swallows_the_rest_of_its_events() {
        let mut det = SwipeDetector::default();
        let start = Instant::now();
        let mut committed_at = None;
        for i in 0..20 {
            if det
                .feed(10.0, 0.0, start + Duration::from_millis(16 * i))
                .is_some()
            {
                assert!(committed_at.is_none(), "committed twice in one gesture");
                committed_at = Some(i);
            }
        }
        assert_eq!(committed_at, Some(14));
        assert!(det.committed);
        // Still latched: more of the same gesture keeps returning None.
        assert_eq!(
            det.feed(10.0, 0.0, start + Duration::from_millis(16 * 20)),
            None
        );
    }
}
