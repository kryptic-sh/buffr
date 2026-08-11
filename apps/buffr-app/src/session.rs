//! Session restore — persist the open tab list across runs.
//!
//! Layout: a tiny JSON blob at `~/.local/share/buffr/session.json`
//! (resolved via `hjkl_config::data_dir`, XDG on every platform):
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

/// Filter a session's entries through `keep`, re-basing `active` onto
/// the kept list. The saved `active` index refers to the *pre-filter*
/// [`Session::entries`] order, so every dropped entry before it shifts
/// the active tab down by one; an adjusted index past the end of the
/// kept list comes back `None` and the restorer falls back to tab 0
/// (§20-2).
pub fn filter_entries<'a, F>(
    entries: impl Iterator<Item = (&'a str, bool)>,
    active: Option<usize>,
    mut keep: F,
) -> (Vec<(&'a str, bool)>, Option<usize>)
where
    F: FnMut(&str) -> bool,
{
    let mut dropped_before_active = 0usize;
    let kept: Vec<(&'a str, bool)> = entries
        .enumerate()
        .filter_map(|(i, (url, pinned))| {
            if keep(url) {
                Some((url, pinned))
            } else {
                if active.is_some_and(|a| i < a) {
                    dropped_before_active += 1;
                }
                None
            }
        })
        .collect();
    let adjusted = active.map(|a| a.saturating_sub(dropped_before_active));
    let kept_len = kept.len();
    (kept, adjusted.filter(|&a| a < kept_len))
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
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restricting permissions on {}", tmp.display()))?;
    }
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
    fn filter_entries_rebases_active_across_dropped_entries() {
        // §20-2 repro: a dropped entry before the saved active index
        // shifts it down by one.
        let mut s = Session::from_tabs([
            ("https://a.example/", false),
            ("javascript:alert(1)", false),
            ("https://b.example/", false),
        ]);
        s.active = Some(2);
        let (kept, active) = filter_entries(s.entries(), s.active, |u| {
            !matches!(u, "javascript:alert(1)")
        });
        assert_eq!(
            kept.iter().map(|(u, _)| *u).collect::<Vec<_>>(),
            vec!["https://a.example/", "https://b.example/"]
        );
        assert_eq!(active, Some(1), "active should land on b.example");
    }

    #[test]
    fn filter_entries_drops_after_active_do_not_shift_it() {
        let mut s = Session::from_tabs([
            ("https://a.example/", false),
            ("https://b.example/", false),
            ("javascript:alert(1)", false),
        ]);
        s.active = Some(1);
        let (kept, active) =
            filter_entries(s.entries(), s.active, |u| !u.starts_with("javascript:"));
        assert_eq!(kept.len(), 2);
        assert_eq!(active, Some(1), "drop after active is index-neutral");
    }

    #[test]
    fn filter_entries_adjusted_index_out_of_range_becomes_none() {
        // Two drops before `active`, but only one slot to absorb them:
        // the adjusted index points past the kept list, so restore
        // falls back to tab 0.
        let mut s = Session::from_tabs([
            ("https://a.example/", false),
            ("javascript:alert(1)", false),
            ("https://b.example/", false),
            ("javascript:alert(2)", false),
        ]);
        s.active = Some(3);
        let (kept, active) =
            filter_entries(s.entries(), s.active, |u| !u.starts_with("javascript:"));
        assert_eq!(kept.len(), 2);
        assert_eq!(active, None);
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

    #[cfg(unix)]
    #[test]
    fn write_restricts_session_file_to_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = default_path(dir.path());
        let s = Session::default();
        write(&path, &s).unwrap();
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o077,
            0,
            "session file is group/other accessible ({mode:o})"
        );
        assert_eq!(mode & 0o600, 0o600, "session file lost owner rw ({mode:o})");
    }
}
