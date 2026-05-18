//! Telemetry **no-op** + opt-in usage counters.
//!
//! ## Contract
//!
//! buffr **never** sends telemetry to a network endpoint. Not now, not
//! ever, not even to a kryptic-owned server. The only telemetry surface
//! that exists is a local JSON file of integer counters that the user
//! has explicitly opted into via `[privacy] enable_telemetry = true`.
//!
//! When `enabled = false` (the default), every method on
//! [`UsageCounters`] is a silent no-op. No file is created. No memory
//! is allocated for counter state. No tracing breadcrumb is emitted.
//!
//! When `enabled = true`, increments accumulate in a
//! `Mutex<HashMap<String, u64>>` and flush to disk via `serde_json` so
//! the user can inspect / diff / submit the file by hand. The format
//! is human-readable on purpose — there is no buffr-side script that
//! reads it; the user owns the lifecycle.
//!
//! ## Counter keys
//!
//! Defined as `pub const`s so callers don't typo a key:
//!
//! - [`KEY_APP_STARTS`]
//! - [`KEY_TABS_OPENED`]
//! - [`KEY_PAGES_LOADED`]
//! - [`KEY_SEARCHES_RUN`]
//! - [`KEY_BOOKMARKS_ADDED`]
//! - [`KEY_DOWNLOADS_COMPLETED`]
//!
//! ## File shape
//!
//! `~/.local/share/buffr/usage-counters.json`:
//!
//! ```json
//! {
//!   "app_starts": 3,
//!   "tabs_opened": 17
//! }
//! ```
//!
//! Pretty-printed JSON. Keys with zero count are omitted. Top-level
//! object only — no schema version, no metadata, no host fingerprint.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Counter key: incremented once per `cef::initialize` success.
pub const KEY_APP_STARTS: &str = "app_starts";
/// Counter key: incremented per [`crate::BrowserHost::open_tab`] call.
pub const KEY_TABS_OPENED: &str = "tabs_opened";
/// Counter key: incremented per `LoadHandler::on_load_end` for the
/// main frame (sub-frames are excluded — same gate as the history
/// recorder).
pub const KEY_PAGES_LOADED: &str = "pages_loaded";
/// Counter key: incremented when the omnibar resolver falls through to
/// the search-engine template (i.e. the user input was not a URL or a
/// host-shaped string).
pub const KEY_SEARCHES_RUN: &str = "searches_run";
/// Counter key: incremented per [`buffr_bookmarks::Bookmarks::add`]
/// success.
pub const KEY_BOOKMARKS_ADDED: &str = "bookmarks_added";
/// Counter key: incremented per
/// [`buffr_downloads::Downloads::record_completed`] success.
pub const KEY_DOWNLOADS_COMPLETED: &str = "downloads_completed";

/// Errors surfaced from disk I/O. Increments themselves never fail
/// (they're in-memory).
#[derive(Debug, Error)]
pub enum TelemetryError {
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
    #[error("counters mutex poisoned")]
    Poisoned,
}

/// Local-only opt-in usage counters.
///
/// Construct via [`UsageCounters::open`]; pass `enabled = false` when
/// `[privacy] enable_telemetry` is unset (the default) and every
/// method becomes a no-op.
pub struct UsageCounters {
    /// On-disk path. Always set so the disabled path can still answer
    /// "where would this go?" for `--telemetry-status`.
    path: PathBuf,
    /// Master switch. `false` makes every public method a no-op.
    enabled: bool,
    /// Accumulator. Empty when `enabled = false`.
    counts: Mutex<HashMap<String, u64>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct OnDisk(HashMap<String, u64>);

impl UsageCounters {
    /// Open the counter store at `path`.
    ///
    /// When `enabled = true`, reads any pre-existing JSON so the new
    /// session continues from the prior totals. When `enabled = false`,
    /// the file is left untouched and the in-memory map stays empty.
    pub fn open(path: impl AsRef<Path>, enabled: bool) -> Self {
        let path = path.as_ref().to_path_buf();
        if !enabled {
            return Self {
                path,
                enabled: false,
                counts: Mutex::new(HashMap::new()),
            };
        }
        let counts = match read_from_disk(&path) {
            Ok(map) => map,
            Err(err) => {
                tracing::warn!(error = %err, path = %path.display(), "telemetry: read failed; starting fresh");
                HashMap::new()
            }
        };
        Self {
            path,
            enabled: true,
            counts: Mutex::new(counts),
        }
    }

    /// Path the counters serialize to. Useful for `--telemetry-status`.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether telemetry is on.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Add 1 to `key`. No-op if disabled.
    pub fn increment(&self, key: &str) {
        if !self.enabled {
            return;
        }
        if let Ok(mut g) = self.counts.lock() {
            let entry = g.entry(key.to_string()).or_insert(0);
            *entry = entry.saturating_add(1);
        }
    }

    /// Persist current state to disk. No-op if disabled.
    ///
    /// Errors log at WARN and are swallowed — telemetry must never
    /// break the calling thread. Use [`UsageCounters::flush_strict`]
    /// when you want errors surfaced (tests).
    pub fn flush(&self) {
        if let Err(err) = self.flush_strict() {
            // Disabled stores return Ok so this only fires when an
            // enabled store fails to write — which is genuinely worth
            // the WARN noise.
            tracing::warn!(error = %err, "telemetry: flush failed");
        }
    }

    /// Strict variant of [`UsageCounters::flush`]. Returns the
    /// underlying I/O / JSON error so tests can assert on it.
    pub fn flush_strict(&self) -> Result<(), TelemetryError> {
        if !self.enabled {
            return Ok(());
        }
        let snapshot: HashMap<String, u64> = match self.counts.lock() {
            Ok(g) => g.clone(),
            Err(_) => return Err(TelemetryError::Poisoned),
        };
        write_to_disk(&self.path, &snapshot)
    }

    /// Snapshot of current counts. Always returns the live in-memory
    /// state when enabled, the empty map otherwise — never re-reads
    /// from disk.
    pub fn read(&self) -> Result<HashMap<String, u64>, TelemetryError> {
        if !self.enabled {
            return Ok(HashMap::new());
        }
        self.counts
            .lock()
            .map(|g| g.clone())
            .map_err(|_| TelemetryError::Poisoned)
    }

    /// Reset every counter. Truncates the on-disk JSON to `{}`.
    /// No-op if disabled (the file was never written).
    pub fn reset(&self) -> Result<(), TelemetryError> {
        if !self.enabled {
            return Ok(());
        }
        if let Ok(mut g) = self.counts.lock() {
            g.clear();
        } else {
            return Err(TelemetryError::Poisoned);
        }
        write_to_disk(&self.path, &HashMap::new())
    }
}

fn read_from_disk(path: &Path) -> Result<HashMap<String, u64>, TelemetryError> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let raw = std::fs::read(path).map_err(|source| TelemetryError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if raw.is_empty() {
        return Ok(HashMap::new());
    }
    let on_disk: OnDisk = serde_json::from_slice(&raw).map_err(|source| TelemetryError::Json {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(on_disk.0)
}

fn write_to_disk(path: &Path, counts: &HashMap<String, u64>) -> Result<(), TelemetryError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|source| TelemetryError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let on_disk = OnDisk(counts.clone());
    let json = serde_json::to_string_pretty(&on_disk).map_err(|source| TelemetryError::Json {
        path: path.to_path_buf(),
        source,
    })?;
    std::fs::write(path, json).map_err(|source| TelemetryError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn disabled_increment_is_noop_no_file() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("usage-counters.json");
        let c = UsageCounters::open(&p, false);
        c.increment(KEY_APP_STARTS);
        c.increment(KEY_TABS_OPENED);
        c.flush();
        assert!(!p.exists(), "disabled store must not write to disk");
        let snapshot = c.read().unwrap();
        assert!(snapshot.is_empty());
    }

    #[test]
    fn enabled_increment_then_flush_persists() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("usage-counters.json");
        let c = UsageCounters::open(&p, true);
        c.increment(KEY_APP_STARTS);
        c.increment(KEY_TABS_OPENED);
        c.increment(KEY_TABS_OPENED);
        c.flush_strict().unwrap();
        assert!(p.exists());
        let raw = std::fs::read_to_string(&p).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v[KEY_APP_STARTS].as_u64(), Some(1));
        assert_eq!(v[KEY_TABS_OPENED].as_u64(), Some(2));
    }

    #[test]
    fn enabled_round_trip_across_open() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("usage-counters.json");
        {
            let c = UsageCounters::open(&p, true);
            c.increment(KEY_PAGES_LOADED);
            c.increment(KEY_PAGES_LOADED);
            c.increment(KEY_SEARCHES_RUN);
            c.flush_strict().unwrap();
        }
        let c2 = UsageCounters::open(&p, true);
        let snap = c2.read().unwrap();
        assert_eq!(snap.get(KEY_PAGES_LOADED), Some(&2));
        assert_eq!(snap.get(KEY_SEARCHES_RUN), Some(&1));
    }

    #[test]
    fn disabled_then_enabled_starts_from_existing_disk_file() {
        // First session: telemetry on, write a few counts.
        let dir = tempdir().unwrap();
        let p = dir.path().join("usage-counters.json");
        {
            let c = UsageCounters::open(&p, true);
            c.increment(KEY_APP_STARTS);
            c.flush_strict().unwrap();
        }
        // User flips telemetry off mid-life: the existing file is
        // untouched, increments don't apply, snapshot is empty.
        let c = UsageCounters::open(&p, false);
        c.increment(KEY_APP_STARTS);
        c.flush();
        assert!(c.read().unwrap().is_empty());
        // The on-disk file is preserved.
        let raw = std::fs::read_to_string(&p).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v[KEY_APP_STARTS].as_u64(), Some(1));
    }

    #[test]
    fn reset_truncates_in_memory_and_on_disk() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("usage-counters.json");
        let c = UsageCounters::open(&p, true);
        c.increment(KEY_BOOKMARKS_ADDED);
        c.increment(KEY_DOWNLOADS_COMPLETED);
        c.flush_strict().unwrap();
        c.reset().unwrap();
        assert!(c.read().unwrap().is_empty());
        let raw = std::fs::read_to_string(&p).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(v.as_object().unwrap().is_empty());
    }

    #[test]
    fn reset_disabled_is_noop_ok() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("usage-counters.json");
        let c = UsageCounters::open(&p, false);
        // Even if the file existed, disabled reset must not touch it.
        std::fs::write(&p, b"{\"app_starts\": 99}").unwrap();
        c.reset().unwrap();
        let raw = std::fs::read_to_string(&p).unwrap();
        assert!(raw.contains("99"));
    }

    #[test]
    fn flush_creates_parent_directory() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("nested/sub/usage-counters.json");
        let c = UsageCounters::open(&p, true);
        c.increment(KEY_APP_STARTS);
        c.flush_strict().unwrap();
        assert!(p.exists());
    }

    #[test]
    fn corrupt_json_recovers_to_empty_state() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("usage-counters.json");
        std::fs::write(&p, b"{not json").unwrap();
        let c = UsageCounters::open(&p, true);
        // Corrupt-on-open does not propagate; counters start empty.
        assert!(c.read().unwrap().is_empty());
        c.increment(KEY_APP_STARTS);
        c.flush_strict().unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v[KEY_APP_STARTS].as_u64(), Some(1));
    }

    #[test]
    fn multiple_increments_same_key_accumulate() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("usage-counters.json");
        let c = UsageCounters::open(&p, true);
        for _ in 0..1000 {
            c.increment(KEY_APP_STARTS);
        }
        let snap = c.read().unwrap();
        assert_eq!(snap.get(KEY_APP_STARTS), Some(&1000));
    }

    #[test]
    fn path_accessor_returns_input_path() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("x.json");
        let c = UsageCounters::open(&p, false);
        assert_eq!(c.path(), p);
        assert!(!c.enabled());
    }
}
