//! Session restore — persist the open tab list across runs.
//!
//! Layout: a tiny JSON blob at `~/.local/share/buffr/session.json`
//! (resolved via `directories::ProjectDirs`):
//!
//! ```json
//! {
//!   "version": 1,
//!   "tabs": [
//!     { "url": "https://example.com", "pinned": false },
//!     { "url": "https://kryptic.sh", "pinned": true }
//!   ]
//! }
//! ```
//!
//! On startup `buffr` reads this file (unless `--no-restore`); on
//! clean shutdown it writes the live tab list. `--list-session`
//! prints the saved file's resolved entries to stdout and exits.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// On-disk session schema version. Bump on incompatible changes.
pub const SCHEMA_VERSION: u32 = 1;

/// One persisted tab. The runtime carries more state (find query,
/// hint session, scroll position) but only `url + pinned` survive a
/// restart — Phase 5 explicitly punts scroll restoration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistedTab {
    pub url: String,
    #[serde(default)]
    pub pinned: bool,
}

/// On-disk session blob. `version` lets us reject older shapes if
/// the format ever changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Session {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub tabs: Vec<PersistedTab>,
    /// Index of the active tab when the session was saved. `None` for
    /// older session files that didn't track focus; the restorer falls
    /// back to tab 0 in that case.
    #[serde(default)]
    pub active: Option<usize>,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            tabs: Vec::new(),
            active: None,
        }
    }
}

fn default_version() -> u32 {
    SCHEMA_VERSION
}

impl Session {
    /// Build a session from an iterator of `(url, pinned)` pairs —
    /// the runtime's preferred shape.
    pub fn from_tabs<'a, I>(tabs: I) -> Self
    where
        I: IntoIterator<Item = (&'a str, bool)>,
    {
        Self {
            version: SCHEMA_VERSION,
            tabs: tabs
                .into_iter()
                .map(|(url, pinned)| PersistedTab {
                    url: url.to_string(),
                    pinned,
                })
                .collect(),
            active: None,
        }
    }

    /// Like [`from_tabs`] but also records the active tab index.
    pub fn from_tabs_with_active<'a, I>(tabs: I, active: Option<usize>) -> Self
    where
        I: IntoIterator<Item = (&'a str, bool)>,
    {
        let mut s = Self::from_tabs(tabs);
        s.active = active;
        s
    }
}

/// Default path: `<data_dir>/session.json` where `<data_dir>` matches
/// `buffr_core::profile_paths().data`.
pub fn default_path(data_dir: &Path) -> PathBuf {
    data_dir.join("session.json")
}

/// Read the session at `path`. Returns `Ok(None)` when the file is
/// absent (legitimate fresh-install state).
pub fn read(path: &Path) -> Result<Option<Session>> {
    match std::fs::read_to_string(path) {
        Ok(s) => {
            let session: Session = serde_json::from_str(&s)
                .with_context(|| format!("parsing session file {}", path.display()))?;
            if session.version != SCHEMA_VERSION {
                warn!(
                    saved = session.version,
                    expected = SCHEMA_VERSION,
                    "session: schema version mismatch — ignoring file",
                );
                return Ok(None);
            }
            Ok(Some(session))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading session file {}", path.display())),
    }
}

/// Atomically write `session` to `path`. Parent dir is created on
/// demand. We `write_all` to a sibling tempfile then `rename`, so a
/// crash mid-write can't corrupt the previous good state.
pub fn write(path: &Path, session: &Session) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating session parent directory {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(session).context("serializing session JSON")?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
    info!(path = %path.display(), tabs = session.tabs.len(), "session: persisted");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_three_tabs() {
        let dir = tempfile::tempdir().unwrap();
        let path = default_path(dir.path());
        let s = Session::from_tabs([
            ("https://a.example", false),
            ("https://b.example", true),
            ("https://c.example", false),
        ]);
        write(&path, &s).unwrap();
        let back = read(&path).unwrap().unwrap();
        assert_eq!(back.version, SCHEMA_VERSION);
        assert_eq!(back.tabs.len(), 3);
        assert_eq!(back.tabs[0].url, "https://a.example");
        assert!(!back.tabs[0].pinned);
        assert!(back.tabs[1].pinned);
    }

    #[test]
    fn read_absent_file_yields_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = default_path(dir.path());
        let r = read(&path).unwrap();
        assert!(r.is_none());
    }

    #[test]
    fn schema_version_mismatch_treated_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = default_path(dir.path());
        std::fs::write(&path, r#"{"version":99,"tabs":[]}"#).unwrap();
        let r = read(&path).unwrap();
        assert!(r.is_none());
    }

    #[test]
    fn deny_unknown_fields_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = default_path(dir.path());
        std::fs::write(&path, r#"{"version":1,"tabs":[],"sneaky":"field"}"#).unwrap();
        let err = read(&path).unwrap_err();
        let s = format!("{err:#}");
        assert!(
            s.contains("sneaky") || s.contains("unknown field"),
            "got: {s}"
        );
    }

    #[test]
    fn pinned_default_false_when_absent() {
        let json = r#"{"version":1,"tabs":[{"url":"https://x"}]}"#;
        let s: Session = serde_json::from_str(json).unwrap();
        assert!(!s.tabs[0].pinned);
    }

    #[test]
    fn from_tabs_preserves_order_and_pin() {
        let s = Session::from_tabs([("https://a", false), ("https://b", true)]);
        assert_eq!(s.tabs.len(), 2);
        assert_eq!(s.tabs[0].url, "https://a");
        assert!(!s.tabs[0].pinned);
        assert!(s.tabs[1].pinned);
    }

    #[test]
    fn write_creates_missing_parent_directory() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a/b/c");
        let path = default_path(&nested);
        let s = Session::from_tabs([("https://x", false)]);
        write(&path, &s).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn empty_session_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = default_path(dir.path());
        let s = Session::default();
        write(&path, &s).unwrap();
        let back = read(&path).unwrap().unwrap();
        assert!(back.tabs.is_empty());
    }

    #[test]
    fn write_atomic_no_temp_remains() {
        let dir = tempfile::tempdir().unwrap();
        let path = default_path(dir.path());
        let s = Session::from_tabs([("https://x", false)]);
        write(&path, &s).unwrap();
        let tmp = path.with_extension("json.tmp");
        assert!(!tmp.exists(), "temp file should have been renamed");
    }
}
