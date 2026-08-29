//! The command-line surface: argument definitions, subcommand
//! dispatch, and the one-shot subcommand implementations.
//!
//! Everything here runs before (and instead of) the event loop: the
//! process prints, exits, and never reaches `AppState`. Keeping the
//! `Cli` definition next to the code that acts on it means adding a
//! flag touches one file.
//!
//! The `open_*_for_cli` helpers each resolve the profile dirs and open
//! one store; they are deliberately not shared with the running app,
//! which keeps its stores open for the process lifetime.
use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;

use buffr_config::{Config, ConfigSource};
use buffr_permissions::Permissions;

use crate::{ensure_profile_data_dir, profile_paths, session};

pub(crate) fn run_check_config(path: Option<&std::path::Path>) -> Result<()> {
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

pub(crate) fn run_print_config(path: Option<&std::path::Path>) -> Result<()> {
    let (cfg, _) = buffr_config::load_and_validate(path).context("loading config")?;
    let s = buffr_config::to_toml_string(&cfg).context("serializing config")?;
    print!("{s}");
    Ok(())
}

/// Resolve the profile data dir and open a store file inside it.
///
/// Shared scaffolding for every `open_*_for_cli` below: resolve the
/// standard profile paths, ensure the data dir exists, then hand the
/// `<data>/<filename>` path to `open` (the store-specific constructor).
pub(crate) fn open_store<T>(
    filename: &str,
    open: impl FnOnce(&std::path::Path) -> Result<T>,
) -> Result<T> {
    let paths = profile_paths().context("resolving profile dirs")?;
    ensure_profile_data_dir(&paths)?;
    open(&paths.data.join(filename))
}

/// Open the bookmarks store at the standard data path. Used by the
/// CLI short-circuits below (no CEF init needed).
pub(crate) fn open_bookmarks_for_cli() -> Result<buffr_bookmarks::Bookmarks> {
    open_store("bookmarks.sqlite", |path| {
        buffr_bookmarks::Bookmarks::open(path).context("opening bookmarks database")
    })
}

pub(crate) fn run_import_bookmarks(path: &std::path::Path) -> Result<()> {
    let html =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let bm = open_bookmarks_for_cli()?;
    let n = bm.import_netscape(&html).context("importing bookmarks")?;
    println!("imported {n} bookmarks");
    Ok(())
}

pub(crate) fn run_list_bookmarks() -> Result<()> {
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

pub(crate) fn run_list_bookmarks_tags() -> Result<()> {
    let bm = open_bookmarks_for_cli()?;
    for tag in bm.all_tags().context("loading tags")? {
        println!("{tag}");
    }
    Ok(())
}

/// Open the downloads store at the standard data path. Used by the
/// CLI short-circuits — no CEF init.
pub(crate) fn open_downloads_for_cli() -> Result<buffr_downloads::Downloads> {
    open_store("downloads.sqlite", |path| {
        buffr_downloads::Downloads::open(path).context("opening downloads database")
    })
}

pub(crate) fn run_list_downloads() -> Result<()> {
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

pub(crate) fn run_clear_completed_downloads() -> Result<()> {
    let dl = open_downloads_for_cli()?;
    let n = dl
        .clear_completed()
        .context("clearing completed downloads")?;
    println!("cleared {n} completed downloads");
    Ok(())
}

/// Open the zoom store at the standard data path. Used by the CLI
/// short-circuits — no CEF init.
pub(crate) fn open_zoom_for_cli() -> Result<buffr_zoom::ZoomStore> {
    open_store("zoom.sqlite", |path| {
        buffr_zoom::ZoomStore::open(path).context("opening zoom database")
    })
}

pub(crate) fn run_list_zoom() -> Result<()> {
    let z = open_zoom_for_cli()?;
    for (domain, level) in z.all().context("loading zoom rows")? {
        println!("{domain}\t{level}");
    }
    Ok(())
}

pub(crate) fn run_clear_zoom() -> Result<()> {
    let z = open_zoom_for_cli()?;
    let n = z.clear().context("clearing zoom rows")?;
    println!("cleared {n} zoom rows");
    Ok(())
}

/// Open the history store at the standard data path. Used by the CLI
/// short-circuits — no CEF init. Skip-schemes only matter for recording,
/// not for reading, so we pass the canonical defaults.
pub(crate) fn open_history_for_cli() -> Result<buffr_history::History> {
    open_store("history.sqlite", |path| {
        buffr_history::History::open(path).context("opening history database")
    })
}

/// `--list-history` / `--search-history` short-circuit.
///
/// When `search` is `Some`, performs a frecency search; otherwise lists
/// the `limit` most-recent visits. Output: one row per visit,
/// tab-separated: `<id>\t<visit_time RFC3339>\t<transition>\t<url>\t<title-or-empty>`.
pub(crate) fn run_query_history(search: Option<&str>, limit: usize) -> Result<()> {
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
pub(crate) fn open_permissions_for_cli() -> Result<Permissions> {
    open_store("permissions.sqlite", |path| {
        Permissions::open(path).context("opening permissions database")
    })
}

pub(crate) fn run_list_permissions() -> Result<()> {
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

pub(crate) fn run_clear_permissions() -> Result<()> {
    let p = open_permissions_for_cli()?;
    let n = p.clear().context("clearing permissions")?;
    println!("cleared {n} permission rows");
    Ok(())
}

pub(crate) fn run_forget_origin(origin: &str) -> Result<()> {
    let p = open_permissions_for_cli()?;
    let n = p
        .forget_origin(origin)
        .context("forgetting permissions for origin")?;
    println!("forgot {n} permission rows for {origin}");
    Ok(())
}

/// Path the [`buffr_core::UsageCounters`] writes to. Stable across
/// callers — `--telemetry-status` and the live runtime resolve here.
pub(crate) fn telemetry_path() -> Result<PathBuf> {
    let paths = profile_paths().context("resolving profile dirs")?;
    ensure_profile_data_dir(&paths)?;
    Ok(paths.data.join("usage-counters.json"))
}

/// Crash report directory. Created lazily on first install.
pub(crate) fn crash_dir() -> Result<PathBuf> {
    let paths = profile_paths().context("resolving profile dirs")?;
    Ok(paths.data.join("crashes"))
}

pub(crate) fn load_config_or_default(path: Option<&std::path::Path>) -> Config {
    match buffr_config::load_and_validate(path) {
        Ok((cfg, _)) => cfg,
        Err(_) => Config::default(),
    }
}

pub(crate) fn run_telemetry_status(config_path: Option<&std::path::Path>) -> Result<()> {
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

pub(crate) fn run_reset_telemetry(config_path: Option<&std::path::Path>) -> Result<()> {
    let cfg = load_config_or_default(config_path);
    let path = telemetry_path()?;
    let counters = buffr_core::UsageCounters::open(&path, cfg.privacy.enable_telemetry);
    counters.reset().context("resetting telemetry counters")?;
    println!("telemetry counters reset");
    Ok(())
}

pub(crate) fn run_list_crashes() -> Result<()> {
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

pub(crate) fn run_purge_crashes(config_path: Option<&std::path::Path>) -> Result<()> {
    let cfg = load_config_or_default(config_path);
    let dir = crash_dir()?;
    let n = buffr_core::CrashReporter::purge_older_than(&dir, cfg.crash_reporter.purge_after_days)
        .context("purging crash reports")?;
    println!("purged {n} reports");
    Ok(())
}

/// Resolve the update-cache path. Stable across the live runtime and
/// the `--check-for-updates` / `--update-status` short-circuits.
pub(crate) fn update_cache_path() -> Result<PathBuf> {
    let paths = profile_paths().context("resolving profile dirs")?;
    ensure_profile_data_dir(&paths)?;
    Ok(paths.data.join("update-cache.json"))
}

pub(crate) fn print_update_status(status: &buffr_core::UpdateStatus) {
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
pub(crate) fn run_check_for_updates(config_path: Option<&std::path::Path>) -> Result<()> {
    let cfg = load_config_or_default(config_path);
    let path = update_cache_path()?;
    let checker = buffr_core::UpdateChecker::new(cfg.updates.clone(), path);
    let status = checker.check_now();
    print_update_status(&status);
    Ok(())
}

pub(crate) fn run_update_status(config_path: Option<&std::path::Path>) -> Result<()> {
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
///
/// Resolves the leader from the user's config rather than assuming one:
/// `<leader>`-prefixed chords print with the character the user will
/// actually press, so the audit matches their keyboard and not a
/// hard-coded guess.
pub(crate) fn run_audit_keymap(path: Option<&std::path::Path>) -> Result<()> {
    let leader = match buffr_config::load_and_validate(path) {
        Ok((cfg, _)) => cfg.general.leader.chars().next().unwrap_or(' '),
        Err(e) => {
            // Audit is a diagnostic — degrade to the built-in default
            // rather than failing outright on an unrelated config error.
            eprintln!("warning: using the default leader ({e})");
            buffr_config::Config::default()
                .general
                .leader
                .chars()
                .next()
                .unwrap_or(' ')
        }
    };
    let rows = buffr_modal::Keymap::audit_default_bindings(leader);
    for (mode, keys, action) in &rows {
        println!("{mode}\t{keys}\t{action:?}");
    }
    Ok(())
}

/// `--list-session` short-circuit. Prints one row per saved tab to
/// stdout: `*\t<url>` when pinned, `\t<url>` otherwise. Schema
/// version is printed on stderr for diagnostic clarity.
pub(crate) fn run_list_session() -> Result<()> {
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

/// ASCII-art banner. Regenerate with:
///
/// ```sh
/// figlet -f "ANSI Regular" buffr > apps/buffr-app/src/art.txt
/// ```
const LONG_ABOUT: &str = concat!(
    "\n",
    include_str!("art.txt"),
    "\nGPU-accelerated CEF web browser · v",
    env!("CARGO_PKG_VERSION"),
);

#[derive(Parser, Debug)]
#[command(name = "buffr-app", version, about = "GPU-accelerated CEF web browser", long_about = LONG_ABOUT)]
pub(crate) struct Cli {
    /// Print resolved config (TOML) to stdout and exit.
    #[arg(long)]
    pub(crate) print_config: bool,
    /// Validate the config file and exit non-zero on failure.
    #[arg(long)]
    pub(crate) check_config: bool,
    /// Override config file path (default: XDG location).
    #[arg(long, value_name = "PATH")]
    pub(crate) config: Option<PathBuf>,
    /// Override `general.homepage` for this run.
    #[arg(long, value_name = "URL")]
    pub(crate) homepage: Option<String>,
    /// Import bookmarks from a Netscape Bookmark File (HTML). Runs
    /// without launching CEF; prints the import count to stdout.
    #[arg(long, value_name = "PATH")]
    pub(crate) import_bookmarks: Option<PathBuf>,
    /// Print every bookmark to stdout and exit. Debug aid until UI lands.
    #[arg(long)]
    pub(crate) list_bookmarks: bool,
    /// Print every bookmark tag (sorted) to stdout and exit.
    #[arg(long)]
    pub(crate) list_bookmarks_tags: bool,
    /// Print every download (most-recent first) to stdout and exit.
    /// Debug aid until the downloads pane lands (Phase 5b chrome).
    #[arg(long)]
    pub(crate) list_downloads: bool,
    /// Delete every `Completed` download row (keeps Failed/Canceled).
    /// Prints the count removed.
    #[arg(long)]
    pub(crate) clear_completed_downloads: bool,
    /// Print every persisted zoom override (`<domain>\t<level>`) and
    /// exit. Debug aid until UI lands.
    #[arg(long)]
    pub(crate) list_zoom: bool,
    /// Wipe the per-site zoom store. Prints the count of rows removed.
    #[arg(long)]
    pub(crate) clear_zoom: bool,
    /// Run in private mode: every store is in-memory, the CEF cache
    /// lives in a tempdir under `$TMPDIR/buffr-private-<pid>` that is
    /// deleted on shutdown. Nothing persists across restarts.
    ///
    /// This is single-window incognito — there is no IPC isolation
    /// from other buffr processes; full-process compartmentalisation
    /// (Tor-Browser-grade) is out of scope for Phase 5.
    #[arg(long)]
    pub(crate) private: bool,
    /// Smoke-test flag for Phase 3 find-in-page wiring. After the
    /// browser is created and the homepage starts loading, kicks off
    /// a single find for `<query>` (forward search). Match counts
    /// are routed through the statusline; tracing also logs each
    /// `OnFindResult` tick so the smoke job can scrape them.
    #[arg(long, value_name = "QUERY")]
    pub(crate) find: Option<String>,
    /// Open this URL in an extra tab on launch. Repeatable; tabs are
    /// added in order after any restored session and the homepage.
    #[arg(long = "new-tab", value_name = "URL", action = clap::ArgAction::Append)]
    pub(crate) new_tab: Vec<String>,
    /// Skip session restore for this run. The homepage opens in a
    /// single tab and a fresh session file is written on exit.
    #[arg(long)]
    pub(crate) no_restore: bool,
    /// Print the saved session (one URL per line, `*` prefix for
    /// pinned tabs) to stdout and exit. Does not launch CEF.
    #[arg(long)]
    pub(crate) list_session: bool,
    /// Print every persisted permission decision and exit.
    /// Output: `<origin>\t<capability>\t<decision>\t<set_at>`.
    #[arg(long)]
    pub(crate) list_permissions: bool,
    /// Wipe the permissions table. Prints the count of rows removed.
    #[arg(long)]
    pub(crate) clear_permissions: bool,
    /// Drop every stored permission decision for `<ORIGIN>`. Prints
    /// the count of rows removed.
    #[arg(long, value_name = "ORIGIN")]
    pub(crate) forget_origin: Option<String>,
    /// Print every history entry (most-recent first) and exit.
    /// Debug aid until the history UI lands. See also `--history-limit`.
    #[arg(long)]
    pub(crate) list_history: bool,
    /// Frecency-search history for `<QUERY>` and print matches, best
    /// first. Mutually exclusive with `--list-history` (search wins).
    #[arg(long, value_name = "QUERY")]
    pub(crate) search_history: Option<String>,
    /// Maximum rows returned by `--list-history` / `--search-history`.
    /// Defaults to 100.
    #[arg(long, value_name = "N")]
    pub(crate) history_limit: Option<usize>,
    /// Print the telemetry on/off state, the on-disk counter file
    /// path, and the current counter table; exit 0. No CEF init.
    #[arg(long)]
    pub(crate) telemetry_status: bool,
    /// Reset every counter to zero (truncates the on-disk JSON to
    /// `{}`). No-op when telemetry is disabled. Prints "telemetry
    /// counters reset" and exits 0.
    #[arg(long)]
    pub(crate) reset_telemetry: bool,
    /// Print every captured panic report (most recent first) and
    /// exit 0. No CEF init.
    #[arg(long)]
    pub(crate) list_crashes: bool,
    /// Delete crash reports older than `crash_reporter.purge_after_days`.
    /// Prints "purged N reports" and exits 0.
    #[arg(long)]
    pub(crate) purge_crashes: bool,
    /// Phase 6 update channel: hit GitHub releases now, print the
    /// resolved status, exit 0. No CEF init. Honors
    /// `[updates] enabled = false` (prints `disabled` without any
    /// network call).
    #[arg(long)]
    pub(crate) check_for_updates: bool,
    /// Read the on-disk update cache and print the cached status. No
    /// network. No CEF init. The statusline reads the same cache.
    #[arg(long)]
    pub(crate) update_status: bool,
    /// Print every default-bound `PageAction` and the keys that bind
    /// it. Exits 0 — used to verify keyboard-only paths for the a11y
    /// audit. No CEF init.
    #[arg(long)]
    pub(crate) audit_keymap: bool,
    /// Override the default engine backend for this run. Synthesises a single
    /// instance with the chosen backend and routes every tab through it,
    /// ignoring `[engines]` config. Valid values: cef.
    #[arg(long, value_name = "NAME")]
    pub(crate) engine: Option<String>,
    /// URLs to open. Each becomes a new tab. When another buffr instance is
    /// already running on the same profile, these are forwarded to it and
    /// this process exits 0. Combined with `--new-tab` URLs for forwarding.
    #[arg(value_name = "URL")]
    pub(crate) urls: Vec<String>,
    /// Hidden smoke-test flag: launch the event loop, wait for the
    /// first `WindowEvent::RedrawRequested` (proves the windowing
    /// backend reached steady state and the compositor / window
    /// manager accepted the surface), then exit 0. Bounded by
    /// `--smoke-test-timeout-ms` (default 30 000) so CI never hangs.
    /// Used by the cross-platform CI smoke harness to catch
    /// regressions in the wayr / winit backends that don't surface
    /// at compile time.
    #[arg(long, hide = true)]
    pub(crate) smoke_test: bool,
    /// Smoke-test timeout in milliseconds. Process exits non-zero if
    /// no `RedrawRequested` event arrives within this budget. Only
    /// honoured when `--smoke-test` is set.
    #[arg(long, default_value = "30000", hide = true)]
    pub(crate) smoke_test_timeout_ms: u64,
}

/// Run the first matching one-shot subcommand, if any.
///
/// Returns `Some(result)` when a subcommand handled the invocation --
/// the caller returns it directly and never initialises CEF -- or
/// `None` to continue into normal browser startup.
///
/// Order is significant: the first matching flag wins, so passing two
/// subcommand flags runs only the earlier one rather than erroring.
pub(crate) fn dispatch(cli: &Cli) -> Option<Result<()>> {
    if cli.check_config {
        return Some(run_check_config(cli.config.as_deref()));
    }
    if cli.print_config {
        return Some(run_print_config(cli.config.as_deref()));
    }
    if let Some(path) = cli.import_bookmarks.as_deref() {
        return Some(run_import_bookmarks(path));
    }
    if cli.list_bookmarks {
        return Some(run_list_bookmarks());
    }
    if cli.list_bookmarks_tags {
        return Some(run_list_bookmarks_tags());
    }
    if cli.list_downloads {
        return Some(run_list_downloads());
    }
    if cli.clear_completed_downloads {
        return Some(run_clear_completed_downloads());
    }
    if cli.list_zoom {
        return Some(run_list_zoom());
    }
    if cli.clear_zoom {
        return Some(run_clear_zoom());
    }
    if cli.list_session {
        return Some(run_list_session());
    }
    if cli.list_permissions {
        return Some(run_list_permissions());
    }
    if cli.clear_permissions {
        return Some(run_clear_permissions());
    }
    if let Some(origin) = cli.forget_origin.as_deref() {
        return Some(run_forget_origin(origin));
    }
    if cli.telemetry_status {
        return Some(run_telemetry_status(cli.config.as_deref()));
    }
    if cli.reset_telemetry {
        return Some(run_reset_telemetry(cli.config.as_deref()));
    }
    if cli.list_crashes {
        return Some(run_list_crashes());
    }
    if cli.purge_crashes {
        return Some(run_purge_crashes(cli.config.as_deref()));
    }
    if cli.check_for_updates {
        return Some(run_check_for_updates(cli.config.as_deref()));
    }
    if cli.update_status {
        return Some(run_update_status(cli.config.as_deref()));
    }
    if cli.audit_keymap {
        return Some(run_audit_keymap(cli.config.as_deref()));
    }
    if cli.search_history.is_some() || cli.list_history {
        let limit = cli.history_limit.unwrap_or(100);
        return Some(run_query_history(cli.search_history.as_deref(), limit));
    }
    None
}
