//! Crash-loop detection.
//!
//! Tracks recent startup timestamps in `<data_dir>/launch.json`. If
//! `LOOP_THRESHOLD` startups land inside `LOOP_WINDOW` with no
//! intervening clean exit, the next launch is treated as a crash loop:
//! the caller skips session restore and (optionally) moves the saved
//! `session.json` aside so the killer URL can't restore again.
//!
//! Lifecycle:
//! - `record_start()` at app startup — appends "now", trims old entries,
//!   returns whether we're in a crash loop.
//! - `record_clean_exit()` on graceful shutdown — clears the tracker so
//!   the next launch starts from a clean slate.
//!
//! The file is best-effort: read/write failures degrade to "no crash
//! loop detected" rather than blocking startup. We never want crash
//! detection to itself become a launch failure.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// Window over which startup attempts are considered.
pub const LOOP_WINDOW_SECS: u64 = 60;
/// Number of startups inside the window that constitutes a crash loop.
pub const LOOP_THRESHOLD: usize = 3;

/// On-disk shape. Just a list of unix-epoch-second timestamps for the
/// last few startups. Clean exit truncates to empty.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LaunchLog {
    #[serde(default)]
    attempts: Vec<u64>,
}

/// Default tracker path: `<data_dir>/launch.json`.
pub fn default_path(data_dir: &Path) -> PathBuf {
    data_dir.join("launch.json")
}

/// Append "now" to the launch log and report whether the recent window
/// already contains enough prior attempts to declare a crash loop.
///
/// Returns `true` if `LOOP_THRESHOLD` or more startup attempts (counting
/// the current one) sit inside the trailing `LOOP_WINDOW_SECS` window.
pub fn record_start(path: &Path) -> bool {
    record_start_at(path, now_secs())
}

/// Mark a graceful shutdown by clearing the launch log. Best-effort —
/// a write failure is logged but doesn't propagate.
pub fn record_clean_exit(path: &Path) {
    if let Err(err) = write(path, &LaunchLog::default()) {
        warn!(error = %err, path = %path.display(), "crash_guard: failed to clear launch log");
    }
}

/// Testable variant of [`record_start`] that accepts an explicit "now"
/// timestamp.
pub fn record_start_at(path: &Path, now: u64) -> bool {
    let mut log = read(path).unwrap_or_default();
    let cutoff = now.saturating_sub(LOOP_WINDOW_SECS);
    log.attempts.retain(|t| *t >= cutoff);
    log.attempts.push(now);
    let loop_detected = log.attempts.len() >= LOOP_THRESHOLD;
    if let Err(err) = write(path, &log) {
        warn!(error = %err, path = %path.display(), "crash_guard: failed to update launch log");
        return false;
    }
    if loop_detected {
        warn!(
            attempts = log.attempts.len(),
            window_secs = LOOP_WINDOW_SECS,
            "crash_guard: crash loop detected — recent launches without clean exit",
        );
    } else {
        info!(
            attempts = log.attempts.len(),
            "crash_guard: launch recorded",
        );
    }
    loop_detected
}

/// Move `session_path` aside to `<session_path>.crashed-<unix_ts>`.
/// Used after [`record_start`] returns `true` so the killer URL set
/// isn't restored on the next launch.
///
/// Returns the destination path on success. Absent source file is a
/// no-op (returns `Ok(None)`); other I/O errors propagate.
pub fn quarantine_session(session_path: &Path, now: u64) -> Result<Option<PathBuf>> {
    if !session_path.exists() {
        return Ok(None);
    }
    let mut name = session_path
        .file_name()
        .map(|s| s.to_os_string())
        .unwrap_or_else(|| std::ffi::OsString::from("session.json"));
    name.push(format!(".crashed-{now}"));
    let dest = session_path
        .parent()
        .map(|p| p.join(&name))
        .unwrap_or_else(|| PathBuf::from(&name));
    std::fs::rename(session_path, &dest)
        .with_context(|| format!("renaming {} -> {}", session_path.display(), dest.display()))?;
    warn!(
        from = %session_path.display(),
        to = %dest.display(),
        "crash_guard: quarantined session file after crash loop",
    );
    Ok(Some(dest))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read(path: &Path) -> Result<LaunchLog> {
    let text = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(LaunchLog::default()),
        Err(e) => {
            return Err(e).with_context(|| format!("reading launch log {}", path.display()));
        }
    };
    let log: LaunchLog = serde_json::from_str(&text)
        .with_context(|| format!("parsing launch log {}", path.display()))?;
    Ok(log)
}

fn write(path: &Path, log: &LaunchLog) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating launch-log parent {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(log).context("serializing launch log")?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_launch_not_a_loop() {
        let dir = tempfile::tempdir().unwrap();
        let path = default_path(dir.path());
        let loop_detected = record_start_at(&path, 1000);
        assert!(!loop_detected);
    }

    #[test]
    fn three_within_window_is_loop() {
        let dir = tempfile::tempdir().unwrap();
        let path = default_path(dir.path());
        assert!(!record_start_at(&path, 1000));
        assert!(!record_start_at(&path, 1010));
        assert!(record_start_at(&path, 1020));
    }

    #[test]
    fn three_outside_window_is_not_loop() {
        let dir = tempfile::tempdir().unwrap();
        let path = default_path(dir.path());
        assert!(!record_start_at(&path, 1000));
        assert!(!record_start_at(&path, 1100));
        assert!(!record_start_at(&path, 1200));
    }

    #[test]
    fn clean_exit_resets_counter() {
        let dir = tempfile::tempdir().unwrap();
        let path = default_path(dir.path());
        assert!(!record_start_at(&path, 1000));
        assert!(!record_start_at(&path, 1010));
        record_clean_exit(&path);
        assert!(!record_start_at(&path, 1020));
        assert!(!record_start_at(&path, 1030));
    }

    #[test]
    fn old_entries_trimmed() {
        let dir = tempfile::tempdir().unwrap();
        let path = default_path(dir.path());
        record_start_at(&path, 100);
        record_start_at(&path, 200);
        // 500 is way past LOOP_WINDOW_SECS from earlier entries — both
        // should be trimmed, leaving only the current attempt.
        record_start_at(&path, 500);
        let log = read(&path).unwrap();
        assert_eq!(log.attempts, vec![500]);
    }

    #[test]
    fn quarantine_renames_file() {
        let dir = tempfile::tempdir().unwrap();
        let session_path = dir.path().join("session.json");
        std::fs::write(&session_path, "{}").unwrap();
        let dest = quarantine_session(&session_path, 42).unwrap().unwrap();
        assert!(!session_path.exists());
        assert!(dest.exists());
        assert!(
            dest.file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with(".crashed-42")
        );
    }

    #[test]
    fn quarantine_absent_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let session_path = dir.path().join("session.json");
        let result = quarantine_session(&session_path, 1).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn corrupt_log_treated_as_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = default_path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "not json").unwrap();
        // record_start swallows the parse error via unwrap_or_default
        // and starts a fresh log, so this launch is the first one.
        let loop_detected = record_start_at(&path, 1000);
        assert!(!loop_detected);
        let log = read(&path).unwrap();
        assert_eq!(log.attempts, vec![1000]);
    }
}
