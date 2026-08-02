//! Implementations of the one-shot CLI subcommands.
//!
//! Everything here runs before (and instead of) the event loop: the
//! process prints, exits, and never reaches `AppState`. Grouping them
//! away from `main.rs` keeps the startup path readable.
//!
//! The `open_*_for_cli` helpers each resolve the profile dirs and open
//! one store; they are deliberately not shared with the running app,
//! which keeps its stores open for the process lifetime.

use anyhow::{Context, Result};
use std::path::PathBuf;

use buffr_config::{Config, ConfigSource};
use buffr_permissions::Permissions;

use crate::{profile_paths, session};

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

/// Open the bookmarks store at the standard data path. Used by the
/// CLI short-circuits below (no CEF init needed).
pub(crate) fn open_bookmarks_for_cli() -> Result<buffr_bookmarks::Bookmarks> {
    let paths = profile_paths().context("resolving profile dirs")?;
    std::fs::create_dir_all(&paths.data).context("creating data dir")?;
    let bm = buffr_bookmarks::Bookmarks::open(paths.data.join("bookmarks.sqlite"))
        .context("opening bookmarks database")?;
    Ok(bm)
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
    let paths = profile_paths().context("resolving profile dirs")?;
    std::fs::create_dir_all(&paths.data).context("creating data dir")?;
    let dl = buffr_downloads::Downloads::open(paths.data.join("downloads.sqlite"))
        .context("opening downloads database")?;
    Ok(dl)
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
    let paths = profile_paths().context("resolving profile dirs")?;
    std::fs::create_dir_all(&paths.data).context("creating data dir")?;
    let z = buffr_zoom::ZoomStore::open(paths.data.join("zoom.sqlite"))
        .context("opening zoom database")?;
    Ok(z)
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
    let paths = profile_paths().context("resolving profile dirs")?;
    std::fs::create_dir_all(&paths.data).context("creating data dir")?;
    let p = Permissions::open(paths.data.join("permissions.sqlite"))
        .context("opening permissions database")?;
    Ok(p)
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
    std::fs::create_dir_all(&paths.data).context("creating data dir")?;
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
    std::fs::create_dir_all(&paths.data).context("creating data dir")?;
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
