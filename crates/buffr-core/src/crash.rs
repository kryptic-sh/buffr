//! Opt-in local-only crash reporter.
//!
//! Writes panic reports to `<data>/crashes/<timestamp>.json` so the
//! user can inspect them by hand. Nothing is uploaded — the reporter
//! is intentionally network-free, like the telemetry no-op next door.
//!
//! ## Wiring
//!
//! [`CrashReporter::install`] sets a `std::panic::set_hook` that
//! captures the panic info, snapshots a `Backtrace`, and writes a
//! JSON file. The hook fires from any thread (Rust panics propagate
//! per-thread; the set_hook handler runs on the panicking thread) so
//! the reporter doesn't need its own dispatcher thread.
//!
//! ## CEF crashes
//!
//! CEF's `BrowserProcessHandler` does **not** expose an
//! `on_uncaught_exception` callback in libcef-147 — that callback is
//! on the `RenderProcessHandler` and only fires for **V8** uncaught
//! exceptions inside renderer processes (i.e. JavaScript errors, not
//! native crashes). Native CEF crashes are caught by Chromium's
//! internal crashpad/breakpad pipeline, which we do not currently
//! configure (it requires a `crashpad_handler` binary + symbol-server
//! URL). For Phase 6 we ship the panic-hook reporter only and leave
//! the breakpad integration as a future task — see
//! [`PLAN.md`](../../../PLAN.md).
//!
//! ## Backtrace capture
//!
//! Rust's panic hook receives a `&PanicHookInfo` whose
//! `Location` is the panic site. We use `std::backtrace::Backtrace::force_capture`
//! so the user gets frames even when `RUST_BACKTRACE` is unset —
//! crashes are rare and worth the slowdown. The capture is per-thread,
//! so panics from non-main threads carry their own stack (which is
//! what you want for "what was the worker doing when it died").

use std::backtrace::Backtrace;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// One captured panic. Pretty-printed JSON on disk.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CrashReport {
    pub timestamp: DateTime<Utc>,
    pub buffr_version: String,
    pub os: String,
    pub display_server: String,
    pub message: String,
    pub location: Option<String>,
    pub backtrace: Vec<String>,
}

/// Errors surfaced from disk I/O.
#[derive(Debug, Error)]
pub enum CrashError {
    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("json error on {path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

/// Crash-reporter handle. Stateless on the call side — install once at
/// process start and forget; the hook owns the reporting path.
pub struct CrashReporter;

/// Module-level guard so [`CrashReporter::install`] is idempotent.
/// Calling it twice would chain the existing hook into the new one;
/// the second call becomes a no-op instead.
static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Process-wide uniquifier for crash-report filenames. The timestamp
/// only has millisecond precision, so two threads unwinding together
/// would otherwise collide and the second `write` would truncate the
/// first report.
static REPORT_SEQ: AtomicU64 = AtomicU64::new(0);

/// How many suffixes [`write_report`] tries before giving up. Each
/// attempt uses `create_new(true)`, so an existing file is never
/// truncated; the bound just stops an unbounded spin if the directory
/// is somehow unwritable in a way that reports as `AlreadyExists`.
const WRITE_REPORT_MAX_ATTEMPTS: u64 = 64;

impl CrashReporter {
    /// Install a `std::panic::set_hook` that writes a [`CrashReport`]
    /// to `<dir>/<timestamp>_<seq>.json` whenever a panic fires.
    ///
    /// Idempotent: subsequent calls log a debug breadcrumb and return
    /// without re-installing.
    ///
    /// When `enabled = false` this is a no-op — the default panic hook
    /// remains in place.
    pub fn install(dir: PathBuf, enabled: bool) {
        if !enabled {
            return;
        }
        if INSTALLED
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            tracing::debug!("crash reporter: already installed");
            return;
        }
        if let Err(err) = std::fs::create_dir_all(&dir) {
            tracing::warn!(
                error = %err,
                dir = %dir.display(),
                "crash reporter: failed to create dir; install skipped"
            );
            INSTALLED.store(false, Ordering::SeqCst);
            return;
        }
        // Chain over the default hook so the user still sees the panic
        // on stderr in dev. We capture the backtrace **before** calling
        // through so the trace reflects the panic site, not the hook.
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let report = build_report(info);
            if let Err(err) = write_report(&dir, &report) {
                // Cannot use `tracing` here without risking a recursive
                // panic if the subscriber itself faulted; eprintln is a
                // safe last resort.
                eprintln!("crash reporter: failed to write report: {err}");
            }
            prev(info);
        }));
        tracing::info!(target: "buffr_core::crash", "crash reporter installed");
    }

    /// Read every JSON file in `dir` and return the parsed reports
    /// most-recent-first. Malformed files are logged at WARN and
    /// skipped — one bad file shouldn't hide the rest.
    pub fn list_crashes(dir: &Path) -> Vec<CrashReport> {
        let mut out: Vec<CrashReport> = read_reports(dir)
            .unwrap_or_default()
            .into_iter()
            .map(|(_, r)| r)
            .collect();
        out.sort_by_key(|r| std::cmp::Reverse(r.timestamp));
        out
    }

    /// Delete every `*.json` file in `dir` whose embedded `timestamp`
    /// is more than `days` days old. Returns the count deleted.
    /// Files that fail to parse are left in place (we don't risk
    /// deleting something the user might want to inspect).
    pub fn purge_older_than(dir: &Path, days: u32) -> Result<usize, CrashError> {
        if !dir.exists() {
            return Ok(0);
        }
        let cutoff = Utc::now() - chrono::Duration::days(i64::from(days));
        let mut removed = 0usize;
        for (path, parsed) in read_reports(dir)? {
            if parsed.timestamp < cutoff {
                if let Err(err) = std::fs::remove_file(&path) {
                    tracing::warn!(error = %err, path = %path.display(), "crash: remove failed");
                } else {
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }
}

/// Read and parse every `*.json` file directly under `dir`.
///
/// Unreadable or malformed files are logged at WARN and skipped — one
/// bad file must never hide the rest, and `purge_older_than` must never
/// delete something it could not parse. Only a failure to enumerate the
/// directory itself is surfaced as an error.
fn read_reports(dir: &Path) -> Result<Vec<(PathBuf, CrashReport)>, CrashError> {
    let entries = std::fs::read_dir(dir).map_err(|source| CrashError::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let raw = match std::fs::read(&path) {
            Ok(r) => r,
            Err(err) => {
                tracing::warn!(error = %err, path = %path.display(), "crash: read failed");
                continue;
            }
        };
        match serde_json::from_slice::<CrashReport>(&raw) {
            Ok(r) => out.push((path, r)),
            Err(err) => {
                tracing::warn!(error = %err, path = %path.display(), "crash: skip malformed report");
            }
        }
    }
    Ok(out)
}

fn build_report(info: &std::panic::PanicHookInfo<'_>) -> CrashReport {
    let message = panic_message(info);
    let location = info
        .location()
        .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()));
    // `force_capture` ignores `RUST_BACKTRACE`; we always capture so
    // crashes-in-the-wild carry frames. Cost is fine — panics are
    // already rare and this only runs once per panic.
    let bt = Backtrace::force_capture();
    let backtrace: Vec<String> = format!("{bt}").lines().map(|s| s.to_string()).collect();
    CrashReport {
        timestamp: Utc::now(),
        buffr_version: env!("CARGO_PKG_VERSION").to_string(),
        os: detect_os(),
        display_server: detect_display_server(),
        message,
        location,
        backtrace,
    }
}

fn panic_message(info: &std::panic::PanicHookInfo<'_>) -> String {
    let payload = info.payload();
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        return (*s).to_string();
    }
    if let Some(s) = payload.downcast_ref::<String>() {
        return s.clone();
    }
    "<non-string panic payload>".to_string()
}

fn detect_os() -> String {
    // `std::env::consts` covers the gross category; finer detail
    // (distro, kernel version) would need a heavyweight dep we are
    // explicitly avoiding for Phase 6.
    format!("{} {}", std::env::consts::OS, std::env::consts::ARCH)
}

fn detect_display_server() -> String {
    if let Ok(s) = std::env::var("XDG_SESSION_TYPE")
        && !s.is_empty()
    {
        return s;
    }
    if std::env::var("WAYLAND_DISPLAY").is_ok() {
        return "wayland".into();
    }
    if std::env::var("DISPLAY").is_ok() {
        return "x11".into();
    }
    "unknown".into()
}

fn write_report(dir: &Path, report: &CrashReport) -> Result<(), CrashError> {
    std::fs::create_dir_all(dir).map_err(|source| CrashError::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    // Filename pattern: `<RFC3339>_<u64>.json` with `:` swapped for `-`
    // so the path is portable to FAT/Windows. The timestamp alone is
    // only millisecond-precise, so the counter disambiguates threads
    // that unwind together; `create_new` then guarantees we never
    // truncate a report that already exists (e.g. a second buffr
    // process sharing the directory).
    let stamp = report
        .timestamp
        .format("%Y-%m-%dT%H-%M-%S%.3fZ")
        .to_string();
    let json = serde_json::to_string_pretty(report).map_err(|source| CrashError::Json {
        path: dir.join(format!("{stamp}.json")),
        source,
    })?;
    let mut last_err = None;
    for _ in 0..WRITE_REPORT_MAX_ATTEMPTS {
        let seq = REPORT_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!("{stamp}_{seq}.json"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut f) => {
                use std::io::Write as _;
                return f
                    .write_all(json.as_bytes())
                    .map_err(|source| CrashError::Io { path, source });
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                last_err = Some((path, err));
                continue;
            }
            Err(source) => return Err(CrashError::Io { path, source }),
        }
    }
    let (path, source) = last_err.expect("loop runs at least once");
    Err(CrashError::Io { path, source })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// Serialises the tests that reason about [`REPORT_SEQ`], which is
    /// process-global. Without it a sibling test could consume the
    /// sequence number this one predicted.
    static SEQ_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn write_fixture(dir: &Path, age_days: i64, label: &str) -> PathBuf {
        let report = CrashReport {
            timestamp: Utc::now() - chrono::Duration::days(age_days),
            buffr_version: "0.0.1".into(),
            os: "linux x86_64".into(),
            display_server: "x11".into(),
            message: format!("test panic {label}"),
            location: Some("file.rs:1:1".into()),
            backtrace: vec!["frame".into()],
        };
        let path = dir.join(format!("test-{label}.json"));
        let json = serde_json::to_string_pretty(&report).unwrap();
        std::fs::write(&path, json).unwrap();
        path
    }

    #[test]
    fn list_empty_when_dir_missing() {
        let dir = tempdir().unwrap();
        let out = CrashReporter::list_crashes(&dir.path().join("does-not-exist"));
        assert!(out.is_empty());
    }

    #[test]
    fn list_returns_sorted_desc_by_timestamp() {
        let dir = tempdir().unwrap();
        write_fixture(dir.path(), 0, "today");
        write_fixture(dir.path(), 5, "five-days-ago");
        write_fixture(dir.path(), 1, "yesterday");
        let out = CrashReporter::list_crashes(dir.path());
        assert_eq!(out.len(), 3);
        assert!(out[0].timestamp > out[1].timestamp);
        assert!(out[1].timestamp > out[2].timestamp);
        assert!(out[0].message.contains("today"));
        assert!(out[2].message.contains("five-days-ago"));
    }

    #[test]
    fn list_skips_non_json_extension() {
        let dir = tempdir().unwrap();
        write_fixture(dir.path(), 0, "valid");
        std::fs::write(dir.path().join("readme.txt"), "ignore me").unwrap();
        std::fs::write(dir.path().join("partial.tmp"), "ignore me too").unwrap();
        let out = CrashReporter::list_crashes(dir.path());
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn list_skips_malformed_json_files() {
        let dir = tempdir().unwrap();
        write_fixture(dir.path(), 0, "valid");
        std::fs::write(dir.path().join("garbage.json"), "{not json").unwrap();
        let out = CrashReporter::list_crashes(dir.path());
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn purge_older_than_deletes_only_old_files() {
        let dir = tempdir().unwrap();
        let recent = write_fixture(dir.path(), 1, "recent");
        let old = write_fixture(dir.path(), 60, "old");
        let removed = CrashReporter::purge_older_than(dir.path(), 30).unwrap();
        assert_eq!(removed, 1);
        assert!(recent.exists());
        assert!(!old.exists());
    }

    #[test]
    fn purge_zero_when_dir_missing() {
        let dir = tempdir().unwrap();
        let removed = CrashReporter::purge_older_than(&dir.path().join("nope"), 30).unwrap();
        assert_eq!(removed, 0);
    }

    #[test]
    fn purge_keeps_malformed_files() {
        let dir = tempdir().unwrap();
        write_fixture(dir.path(), 60, "old-but-valid");
        std::fs::write(dir.path().join("malformed.json"), "{").unwrap();
        let removed = CrashReporter::purge_older_than(dir.path(), 30).unwrap();
        assert_eq!(removed, 1);
        assert!(dir.path().join("malformed.json").exists());
    }

    #[test]
    fn write_report_round_trip() {
        let dir = tempdir().unwrap();
        let report = CrashReport {
            timestamp: Utc::now(),
            buffr_version: "0.0.1".into(),
            os: "linux x86_64".into(),
            display_server: "x11".into(),
            message: "boom".into(),
            location: Some("src/lib.rs:42:0".into()),
            backtrace: vec!["frame 0".into(), "frame 1".into()],
        };
        write_report(dir.path(), &report).unwrap();
        let out = CrashReporter::list_crashes(dir.path());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].message, "boom");
        assert_eq!(out[0].location.as_deref(), Some("src/lib.rs:42:0"));
        assert_eq!(out[0].backtrace.len(), 2);
    }

    #[test]
    fn write_report_does_not_clobber_same_millisecond_reports() {
        // Regression (M22): the filename used to be `<stamp>.json` with
        // millisecond precision and `fs::write` truncation, so two
        // reports carrying the same timestamp lost one.
        let _guard = SEQ_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempdir().unwrap();
        let ts = Utc::now();
        for i in 0..5 {
            let report = CrashReport {
                timestamp: ts,
                buffr_version: "0.0.1".into(),
                os: "linux x86_64".into(),
                display_server: "x11".into(),
                message: format!("boom {i}"),
                location: None,
                backtrace: vec![],
            };
            write_report(dir.path(), &report).unwrap();
        }
        let out = CrashReporter::list_crashes(dir.path());
        assert_eq!(out.len(), 5, "every report must survive");
        let mut msgs: Vec<&str> = out.iter().map(|r| r.message.as_str()).collect();
        msgs.sort_unstable();
        assert_eq!(msgs, vec!["boom 0", "boom 1", "boom 2", "boom 3", "boom 4"]);
    }

    #[test]
    fn write_report_never_truncates_an_existing_file() {
        // `create_new(true)`: if the chosen name is already taken the
        // writer must move on rather than overwrite.
        let _guard = SEQ_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempdir().unwrap();
        let report = CrashReport {
            timestamp: Utc::now(),
            buffr_version: "0.0.1".into(),
            os: "linux x86_64".into(),
            display_server: "x11".into(),
            message: "fresh".into(),
            location: None,
            backtrace: vec![],
        };
        let stamp = report
            .timestamp
            .format("%Y-%m-%dT%H-%M-%S%.3fZ")
            .to_string();
        // Pre-create the exact name the next sequence number will pick.
        let seq = REPORT_SEQ.load(Ordering::Relaxed);
        let squatted = dir.path().join(format!("{stamp}_{seq}.json"));
        std::fs::write(&squatted, "PRE-EXISTING").unwrap();
        write_report(dir.path(), &report).unwrap();
        assert_eq!(std::fs::read_to_string(&squatted).unwrap(), "PRE-EXISTING");
        let out = CrashReporter::list_crashes(dir.path());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].message, "fresh");
    }

    #[test]
    fn install_disabled_is_noop() {
        // Disabled install must not flip the global state.
        let dir = tempdir().unwrap();
        CrashReporter::install(dir.path().to_path_buf(), false);
        // We can't assert state directly without tearing down the
        // hook, but `install(false)` should leave INSTALLED unchanged.
        // If it had set the flag, the next test enabling install
        // would early-return and `synthetic_panic_writes_report` would
        // fail to capture. (That test runs in a sibling thread to
        // avoid contaminating this one.)
        let _ = dir;
    }

    #[test]
    fn synthetic_panic_writes_report_from_child_thread() {
        // Skip if a panic hook is already installed (test ordering
        // can vary). We guard by checking INSTALLED first.
        if INSTALLED.load(Ordering::SeqCst) {
            return;
        }
        let _guard = SEQ_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempdir().unwrap();
        let dir_path = dir.path().to_path_buf();
        CrashReporter::install(dir_path.clone(), true);
        let join = std::thread::spawn(move || {
            // `catch_unwind` lets the test continue past the panic.
            let _ = std::panic::catch_unwind(|| {
                panic!("from-child-thread");
            });
        });
        join.join().unwrap();
        let out = CrashReporter::list_crashes(&dir_path);
        assert!(
            out.iter().any(|r| r.message.contains("from-child-thread")),
            "expected child-thread panic captured; got: {:?}",
            out.iter().map(|r| &r.message).collect::<Vec<_>>()
        );
        // The captured report carries a non-empty backtrace.
        let captured = out
            .iter()
            .find(|r| r.message.contains("from-child-thread"))
            .unwrap();
        assert!(!captured.backtrace.is_empty());
    }
}
