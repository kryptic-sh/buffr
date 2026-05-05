//! Session restore — persist the open tab list across runs.
//!
//! Layout: a tiny JSON blob at `~/.local/share/buffr/session.json`
//! (resolved via `directories::ProjectDirs`):
//!
//! ```json
//! {
//!   "version": 1,
//!   "pinned": ["https://kryptic.sh"],
//!   "tabs":   ["https://example.com", "https://other.example"],
//!   "active": 1
//! }
//! ```
//!
//! Pinned and unpinned tabs live in separate arrays so the on-disk
//! split is explicit. Restore opens pinned tabs first, then unpinned;
//! the live tab strip mirrors that ordering. `active` is the index in
//! the combined `pinned ++ tabs` list — `0` is the first pinned tab,
//! `pinned.len()` is the first unpinned tab.
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

/// On-disk session blob. Pinned and unpinned tabs are stored in
/// separate arrays; the runtime tab order is `pinned ++ tabs` and
/// `active` indexes into that combined list.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Session {
    #[serde(default = "default_version")]
    pub version: u32,
    /// Pinned tab URLs in their saved order.
    #[serde(default)]
    pub pinned: Vec<String>,
    /// Unpinned tab URLs in their saved order.
    #[serde(default)]
    pub tabs: Vec<String>,
    /// Index of the active tab when the session was saved, into the
    /// combined `pinned ++ tabs` list. `None` for older session files
    /// that didn't track focus; the restorer falls back to tab 0.
    #[serde(default)]
    pub active: Option<usize>,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            pinned: Vec::new(),
            tabs: Vec::new(),
            active: None,
        }
    }
}

fn default_version() -> u32 {
    SCHEMA_VERSION
}

impl Session {
    /// Build a session from an iterator of `(url, pinned)` pairs in
    /// the runtime tab order. Splits into the two on-disk arrays
    /// preserving relative order within each.
    pub fn from_tabs<'a, I>(tabs: I) -> Self
    where
        I: IntoIterator<Item = (&'a str, bool)>,
    {
        let mut pinned = Vec::new();
        let mut unpinned = Vec::new();
        for (url, is_pinned) in tabs {
            if is_pinned {
                pinned.push(url.to_string());
            } else {
                unpinned.push(url.to_string());
            }
        }
        Self {
            version: SCHEMA_VERSION,
            pinned,
            tabs: unpinned,
            active: None,
        }
    }

    /// Like [`Self::from_tabs`] but also records the active tab index.
    pub fn from_tabs_with_active<'a, I>(tabs: I, active: Option<usize>) -> Self
    where
        I: IntoIterator<Item = (&'a str, bool)>,
    {
        let mut s = Self::from_tabs(tabs);
        s.active = active;
        s
    }

    /// Iterate `(url, pinned)` pairs in the combined runtime order
    /// (pinned first, then unpinned).
    pub fn entries(&self) -> impl Iterator<Item = (&str, bool)> {
        self.pinned
            .iter()
            .map(|u| (u.as_str(), true))
            .chain(self.tabs.iter().map(|u| (u.as_str(), false)))
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
    let text = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(e).with_context(|| format!("reading session file {}", path.display()));
        }
    };
    let session: Session = serde_json::from_str(&text)
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
    info!(
        path = %path.display(),
        pinned = session.pinned.len(),
        tabs = session.tabs.len(),
        "session: persisted",
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_split() {
        let dir = tempfile::tempdir().unwrap();
        let path = default_path(dir.path());
        let s = Session::from_tabs([
            ("https://a.example", false),
            ("https://b.example", true),
            ("https://c.example", false),
            ("https://d.example", true),
        ]);
        write(&path, &s).unwrap();
        let back = read(&path).unwrap().unwrap();
        assert_eq!(back.version, SCHEMA_VERSION);
        assert_eq!(back.pinned, vec!["https://b.example", "https://d.example"]);
        assert_eq!(back.tabs, vec!["https://a.example", "https://c.example"]);
    }

    #[test]
    fn entries_yields_pinned_first() {
        let s = Session::from_tabs([
            ("https://a", false),
            ("https://b", true),
            ("https://c", false),
            ("https://d", true),
        ]);
        let collected: Vec<_> = s.entries().collect();
        assert_eq!(
            collected,
            vec![
                ("https://b", true),
                ("https://d", true),
                ("https://a", false),
                ("https://c", false),
            ]
        );
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
        std::fs::write(&path, r#"{"version":99,"pinned":[],"tabs":[]}"#).unwrap();
        let r = read(&path).unwrap();
        assert!(r.is_none());
    }

    #[test]
    fn empty_session_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = default_path(dir.path());
        let s = Session::default();
        write(&path, &s).unwrap();
        let back = read(&path).unwrap().unwrap();
        assert!(back.pinned.is_empty());
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
