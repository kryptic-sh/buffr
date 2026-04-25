//! SQLite-backed download tracking for buffr (Phase 5).
//!
//! Phase-5 scope: a pure data layer. No UI, no IPC. Mirrors the
//! [`buffr_history`]/[`buffr_bookmarks`] crate shapes —
//! `Mutex<Connection>`, forward-only migrations, no FTS5.
//!
//! # Schema (v1)
//!
//! One `downloads` table; see [`schema`]. `cef_id` is a UNIQUE column
//! and the natural primary key from CEF's perspective — it survives
//! across the `OnBeforeDownload` → many `OnDownloadUpdated` flow that a
//! single download produces, so we upsert on it inside
//! [`Downloads::record_started`].
//!
//! # Behaviour
//!
//! - [`Downloads::record_started`] is **idempotent on `cef_id`**: a
//!   second call with the same `cef_id` returns the existing
//!   [`DownloadId`] without inserting a duplicate row. CEF can fire
//!   `OnBeforeDownload` more than once if the user cancels mid-prompt
//!   and retries.
//! - [`Downloads::update_progress`] only writes if the row's status is
//!   `in_flight`. Terminal states freeze `received_bytes`.
//! - All terminal recorders ([`Downloads::record_completed`],
//!   [`Downloads::record_canceled`], [`Downloads::record_failed`]) set
//!   `finished_at = Utc::now()` in the same transaction.
//! - [`Downloads::clear_completed`] only deletes `Completed` rows —
//!   `Failed`/`Canceled` are kept around for user inspection.
//!
//! # Concurrency
//!
//! [`Downloads`] wraps `Mutex<rusqlite::Connection>`. CEF callbacks fire
//! on the browser thread; the downloads pane (Phase 5b UI) will read
//! through the same handle. Lock contention is not a concern at the
//! per-tick rates CEF emits.

use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::trace;

pub mod schema;

/// Strongly-typed download id. New-type around `i64` so callers can't
/// accidentally pass a history id, bookmark id, or CEF download id
/// where a [`Downloads`] row id is expected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DownloadId(pub i64);

/// Lifecycle status of a download. Stored as a TEXT tag so adding
/// variants is free.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DownloadStatus {
    /// CEF is actively writing bytes to disk.
    InFlight,
    /// CEF reported `is_complete() == true`. `full_path` is set.
    Completed,
    /// CEF reported `is_canceled() == true` or the user cancelled.
    Canceled,
    /// CEF reported `is_interrupted() == true` or the dispatcher
    /// errored out. `failure` carries the reason string.
    Failed,
}

impl DownloadStatus {
    fn as_str(self) -> &'static str {
        match self {
            DownloadStatus::InFlight => "in_flight",
            DownloadStatus::Completed => "completed",
            DownloadStatus::Canceled => "canceled",
            DownloadStatus::Failed => "failed",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "in_flight" => DownloadStatus::InFlight,
            "completed" => DownloadStatus::Completed,
            "canceled" => DownloadStatus::Canceled,
            "failed" => DownloadStatus::Failed,
            _ => DownloadStatus::Failed,
        }
    }
}

/// One row of the `downloads` table, decoded into Rust types.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Download {
    pub id: DownloadId,
    /// CEF's `CefDownloadItem::id`. Used by in-flight callbacks (the
    /// `OnDownloadUpdated` tick) to find the matching row.
    pub cef_id: u32,
    pub url: String,
    pub suggested_name: String,
    pub mime: Option<String>,
    pub total_bytes: Option<u64>,
    pub received_bytes: u64,
    pub status: DownloadStatus,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub full_path: Option<String>,
    pub failure: Option<String>,
}

/// Errors surfaced from [`Downloads`].
#[derive(Debug, Error)]
pub enum DownloadError {
    #[error("opening sqlite database failed")]
    Open {
        #[source]
        source: rusqlite::Error,
    },
    #[error("applying migration v{version} failed")]
    Migrate {
        #[source]
        source: rusqlite::Error,
        version: i64,
    },
    #[error("query failed")]
    Query {
        #[from]
        source: rusqlite::Error,
    },
    #[error("downloads mutex poisoned")]
    Poisoned,
}

/// SQLite-backed downloads store.
pub struct Downloads {
    conn: Mutex<Connection>,
}

impl Downloads {
    /// Open or create the SQLite database at `path` and run any
    /// pending schema migrations.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, DownloadError> {
        let mut conn = Connection::open_with_flags(
            path.as_ref(),
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )
        .map_err(|source| DownloadError::Open { source })?;
        Self::tune(&conn)?;
        schema::apply(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// In-memory database — for tests and short-lived ephemeral
    /// profiles (private windows, Phase 5 follow-up).
    pub fn open_in_memory() -> Result<Self, DownloadError> {
        let mut conn =
            Connection::open_in_memory().map_err(|source| DownloadError::Open { source })?;
        Self::tune(&conn)?;
        schema::apply(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Apply per-connection pragmas. Same shape as the other stores.
    fn tune(conn: &Connection) -> Result<(), DownloadError> {
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|source| DownloadError::Open { source })?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|source| DownloadError::Open { source })?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|source| DownloadError::Open { source })?;
        Ok(())
    }

    /// Record a new in-flight download or return the existing
    /// [`DownloadId`] if `cef_id` already has a row.
    ///
    /// This is the entry point for CEF's `OnBeforeDownload` callback.
    /// CEF can re-fire that callback (cancel + retry, transient
    /// dialogs); we look up by `cef_id` first to avoid duplicates.
    pub fn record_started(
        &self,
        cef_id: u32,
        url: &str,
        suggested_name: &str,
        mime: Option<&str>,
        total_bytes: Option<u64>,
    ) -> Result<DownloadId, DownloadError> {
        let conn = self.conn.lock().map_err(|_| DownloadError::Poisoned)?;
        let existing: Option<i64> = conn
            .query_row(
                "SELECT id FROM downloads WHERE cef_id = ?1",
                params![cef_id as i64],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(id) = existing {
            trace!(
                cef_id,
                id, "downloads: record_started no-op (already exists)"
            );
            return Ok(DownloadId(id));
        }
        let now = Utc::now().timestamp();
        // SQLite stores u64 as i64 — total_bytes never realistically
        // exceeds i64::MAX so we cast through.
        let total_i64 = total_bytes.map(|n| n as i64);
        conn.execute(
            "INSERT INTO downloads \
             (cef_id, url, suggested_name, mime, total_bytes, received_bytes, status, started_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?7)",
            params![
                cef_id as i64,
                url,
                suggested_name,
                mime,
                total_i64,
                DownloadStatus::InFlight.as_str(),
                now,
            ],
        )?;
        Ok(DownloadId(conn.last_insert_rowid()))
    }

    /// Update progress on an in-flight download. No-op if the row is
    /// in a terminal state (CEF occasionally emits a final tick after
    /// `is_complete`).
    pub fn update_progress(
        &self,
        id: DownloadId,
        received_bytes: u64,
        total_bytes: Option<u64>,
    ) -> Result<(), DownloadError> {
        let conn = self.conn.lock().map_err(|_| DownloadError::Poisoned)?;
        let total_i64 = total_bytes.map(|n| n as i64);
        let n = conn.execute(
            "UPDATE downloads SET received_bytes = ?1, \
             total_bytes = COALESCE(?2, total_bytes) \
             WHERE id = ?3 AND status = 'in_flight'",
            params![received_bytes as i64, total_i64, id.0],
        )?;
        if n == 0 {
            trace!(
                id = id.0,
                "downloads: update_progress no-op (terminal or missing)"
            );
        }
        Ok(())
    }

    /// Flip status to [`DownloadStatus::Completed`], stamp
    /// `finished_at`, and record the final on-disk path.
    pub fn record_completed(&self, id: DownloadId, full_path: &Path) -> Result<(), DownloadError> {
        let conn = self.conn.lock().map_err(|_| DownloadError::Poisoned)?;
        let now = Utc::now().timestamp();
        let path_str = full_path.to_string_lossy().into_owned();
        conn.execute(
            "UPDATE downloads SET status = ?1, finished_at = ?2, full_path = ?3 \
             WHERE id = ?4",
            params![DownloadStatus::Completed.as_str(), now, path_str, id.0],
        )?;
        Ok(())
    }

    /// Flip status to [`DownloadStatus::Canceled`] and stamp
    /// `finished_at`.
    pub fn record_canceled(&self, id: DownloadId) -> Result<(), DownloadError> {
        let conn = self.conn.lock().map_err(|_| DownloadError::Poisoned)?;
        let now = Utc::now().timestamp();
        conn.execute(
            "UPDATE downloads SET status = ?1, finished_at = ?2 WHERE id = ?3",
            params![DownloadStatus::Canceled.as_str(), now, id.0],
        )?;
        Ok(())
    }

    /// Flip status to [`DownloadStatus::Failed`], stamp `finished_at`,
    /// record the failure reason.
    pub fn record_failed(&self, id: DownloadId, reason: &str) -> Result<(), DownloadError> {
        let conn = self.conn.lock().map_err(|_| DownloadError::Poisoned)?;
        let now = Utc::now().timestamp();
        conn.execute(
            "UPDATE downloads SET status = ?1, finished_at = ?2, failure = ?3 \
             WHERE id = ?4",
            params![DownloadStatus::Failed.as_str(), now, reason, id.0],
        )?;
        Ok(())
    }

    /// Look up a single download by row id.
    pub fn get(&self, id: DownloadId) -> Result<Option<Download>, DownloadError> {
        let conn = self.conn.lock().map_err(|_| DownloadError::Poisoned)?;
        let row = conn
            .query_row(
                "SELECT id, cef_id, url, suggested_name, mime, total_bytes, \
                 received_bytes, status, started_at, finished_at, full_path, failure \
                 FROM downloads WHERE id = ?1",
                params![id.0],
                row_to_download,
            )
            .optional()?;
        Ok(row)
    }

    /// Look up a download by CEF id. Used by the CEF
    /// `OnDownloadUpdated` handler to find the row matching the live
    /// `DownloadItem`.
    pub fn get_by_cef_id(&self, cef_id: u32) -> Result<Option<Download>, DownloadError> {
        let conn = self.conn.lock().map_err(|_| DownloadError::Poisoned)?;
        let row = conn
            .query_row(
                "SELECT id, cef_id, url, suggested_name, mime, total_bytes, \
                 received_bytes, status, started_at, finished_at, full_path, failure \
                 FROM downloads WHERE cef_id = ?1",
                params![cef_id as i64],
                row_to_download,
            )
            .optional()?;
        Ok(row)
    }

    /// Most-recent-first list of every download, capped at `limit`.
    pub fn all(&self, limit: usize) -> Result<Vec<Download>, DownloadError> {
        let conn = self.conn.lock().map_err(|_| DownloadError::Poisoned)?;
        let mut stmt = conn.prepare(
            "SELECT id, cef_id, url, suggested_name, mime, total_bytes, \
             received_bytes, status, started_at, finished_at, full_path, failure \
             FROM downloads ORDER BY started_at DESC, id DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit as i64], row_to_download)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// All currently in-flight downloads, oldest-first so the
    /// downloads pane (Phase 5b) can show them in start order.
    pub fn in_flight(&self) -> Result<Vec<Download>, DownloadError> {
        let conn = self.conn.lock().map_err(|_| DownloadError::Poisoned)?;
        let mut stmt = conn.prepare(
            "SELECT id, cef_id, url, suggested_name, mime, total_bytes, \
             received_bytes, status, started_at, finished_at, full_path, failure \
             FROM downloads WHERE status = 'in_flight' \
             ORDER BY started_at ASC, id ASC",
        )?;
        let rows = stmt
            .query_map([], row_to_download)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Delete every `Completed` row. Returns the number deleted.
    /// `Failed`/`Canceled` rows are kept so the user can see why.
    pub fn clear_completed(&self) -> Result<usize, DownloadError> {
        let conn = self.conn.lock().map_err(|_| DownloadError::Poisoned)?;
        let n = conn.execute("DELETE FROM downloads WHERE status = 'completed'", [])?;
        Ok(n)
    }

    /// Total row count. Used by tests + diagnostics.
    pub fn count(&self) -> Result<usize, DownloadError> {
        let conn = self.conn.lock().map_err(|_| DownloadError::Poisoned)?;
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM downloads", [], |row| row.get(0))?;
        Ok(n as usize)
    }
}

fn row_to_download(row: &rusqlite::Row<'_>) -> rusqlite::Result<Download> {
    let cef_id_i64: i64 = row.get(1)?;
    let total: Option<i64> = row.get(5)?;
    let received: i64 = row.get(6)?;
    let status_s: String = row.get(7)?;
    let started: i64 = row.get(8)?;
    let finished: Option<i64> = row.get(9)?;
    Ok(Download {
        id: DownloadId(row.get(0)?),
        cef_id: cef_id_i64 as u32,
        url: row.get(2)?,
        suggested_name: row.get(3)?,
        mime: row.get(4)?,
        total_bytes: total.map(|n| n as u64),
        received_bytes: received as u64,
        status: DownloadStatus::parse(&status_s),
        started_at: ts_to_dt(started),
        finished_at: finished.map(ts_to_dt),
        full_path: row.get(10)?,
        failure: row.get(11)?,
    })
}

fn ts_to_dt(secs: i64) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp(secs, 0)
        .unwrap_or_else(|| DateTime::<Utc>::from_timestamp(0, 0).expect("epoch"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn open_in_memory_runs_migrations() {
        let d = Downloads::open_in_memory().unwrap();
        assert_eq!(d.count().unwrap(), 0);
        assert!(d.all(10).unwrap().is_empty());
        assert!(d.in_flight().unwrap().is_empty());
        assert_eq!(schema::latest_version(), 1);
    }

    #[test]
    fn record_started_inserts_in_flight_row() {
        let d = Downloads::open_in_memory().unwrap();
        let id = d
            .record_started(
                42,
                "https://example.com/file.zip",
                "file.zip",
                Some("application/zip"),
                Some(1024),
            )
            .unwrap();
        assert_eq!(d.count().unwrap(), 1);
        let row = d.get(id).unwrap().expect("row exists");
        assert_eq!(row.cef_id, 42);
        assert_eq!(row.url, "https://example.com/file.zip");
        assert_eq!(row.suggested_name, "file.zip");
        assert_eq!(row.mime.as_deref(), Some("application/zip"));
        assert_eq!(row.total_bytes, Some(1024));
        assert_eq!(row.received_bytes, 0);
        assert_eq!(row.status, DownloadStatus::InFlight);
        assert!(row.finished_at.is_none());
        assert!(row.full_path.is_none());
        assert!(row.failure.is_none());
        // `in_flight` returns it.
        let in_flight = d.in_flight().unwrap();
        assert_eq!(in_flight.len(), 1);
        assert_eq!(in_flight[0].id, id);
    }

    #[test]
    fn record_started_idempotent_on_cef_id() {
        let d = Downloads::open_in_memory().unwrap();
        let id1 = d
            .record_started(7, "https://x.test/a", "a", None, None)
            .unwrap();
        let id2 = d
            .record_started(7, "https://x.test/a", "a", None, None)
            .unwrap();
        assert_eq!(id1, id2);
        assert_eq!(d.count().unwrap(), 1);
    }

    #[test]
    fn update_progress_writes_when_in_flight() {
        let d = Downloads::open_in_memory().unwrap();
        let id = d
            .record_started(1, "https://x.test/a", "a", None, Some(100))
            .unwrap();
        d.update_progress(id, 50, Some(200)).unwrap();
        let row = d.get(id).unwrap().unwrap();
        assert_eq!(row.received_bytes, 50);
        assert_eq!(row.total_bytes, Some(200));
        // None passthrough leaves total_bytes alone.
        d.update_progress(id, 75, None).unwrap();
        let row = d.get(id).unwrap().unwrap();
        assert_eq!(row.received_bytes, 75);
        assert_eq!(row.total_bytes, Some(200));
    }

    #[test]
    fn update_progress_noop_on_terminal_status() {
        let d = Downloads::open_in_memory().unwrap();
        let id = d
            .record_started(1, "https://x.test/a", "a", None, None)
            .unwrap();
        d.record_completed(id, &PathBuf::from("/tmp/a")).unwrap();
        d.update_progress(id, 999, Some(999)).unwrap();
        let row = d.get(id).unwrap().unwrap();
        // received_bytes frozen at the post-completion value (0 here).
        assert_eq!(row.received_bytes, 0);
        assert_eq!(row.status, DownloadStatus::Completed);
    }

    #[test]
    fn record_completed_flips_status_and_path() {
        let d = Downloads::open_in_memory().unwrap();
        let id = d
            .record_started(1, "https://x.test/a", "a", None, None)
            .unwrap();
        d.record_completed(id, &PathBuf::from("/tmp/downloads/a"))
            .unwrap();
        let row = d.get(id).unwrap().unwrap();
        assert_eq!(row.status, DownloadStatus::Completed);
        assert_eq!(row.full_path.as_deref(), Some("/tmp/downloads/a"));
        assert!(row.finished_at.is_some());
    }

    #[test]
    fn record_failed_flips_status_with_reason() {
        let d = Downloads::open_in_memory().unwrap();
        let id = d
            .record_started(1, "https://x.test/a", "a", None, None)
            .unwrap();
        d.record_failed(id, "network error").unwrap();
        let row = d.get(id).unwrap().unwrap();
        assert_eq!(row.status, DownloadStatus::Failed);
        assert_eq!(row.failure.as_deref(), Some("network error"));
        assert!(row.finished_at.is_some());
    }

    #[test]
    fn record_canceled_flips_status() {
        let d = Downloads::open_in_memory().unwrap();
        let id = d
            .record_started(1, "https://x.test/a", "a", None, None)
            .unwrap();
        d.record_canceled(id).unwrap();
        let row = d.get(id).unwrap().unwrap();
        assert_eq!(row.status, DownloadStatus::Canceled);
        assert!(row.finished_at.is_some());
    }

    #[test]
    fn clear_completed_keeps_failed_and_in_flight() {
        let d = Downloads::open_in_memory().unwrap();
        let in_flight_id = d
            .record_started(1, "https://x.test/a", "a", None, None)
            .unwrap();
        let completed_id = d
            .record_started(2, "https://x.test/b", "b", None, None)
            .unwrap();
        d.record_completed(completed_id, &PathBuf::from("/tmp/b"))
            .unwrap();
        let failed_id = d
            .record_started(3, "https://x.test/c", "c", None, None)
            .unwrap();
        d.record_failed(failed_id, "ouch").unwrap();
        let canceled_id = d
            .record_started(4, "https://x.test/d", "d", None, None)
            .unwrap();
        d.record_canceled(canceled_id).unwrap();

        let removed = d.clear_completed().unwrap();
        assert_eq!(removed, 1);
        assert!(d.get(completed_id).unwrap().is_none());
        assert!(d.get(in_flight_id).unwrap().is_some());
        assert!(d.get(failed_id).unwrap().is_some());
        assert!(d.get(canceled_id).unwrap().is_some());
    }

    #[test]
    fn all_orders_most_recent_first() {
        let d = Downloads::open_in_memory().unwrap();
        let id1 = d
            .record_started(1, "https://x.test/a", "a", None, None)
            .unwrap();
        let id2 = d
            .record_started(2, "https://x.test/b", "b", None, None)
            .unwrap();
        let id3 = d
            .record_started(3, "https://x.test/c", "c", None, None)
            .unwrap();
        let all = d.all(10).unwrap();
        assert_eq!(all.len(), 3);
        // Tie-break is `id DESC` since started_at can equal at second
        // resolution — the most-recently-inserted comes first.
        assert_eq!(all[0].id, id3);
        assert_eq!(all[1].id, id2);
        assert_eq!(all[2].id, id1);
    }

    #[test]
    fn get_by_cef_id_returns_matching_row() {
        let d = Downloads::open_in_memory().unwrap();
        let id = d
            .record_started(99, "https://x.test/a", "a", None, None)
            .unwrap();
        let row = d.get_by_cef_id(99).unwrap().expect("exists");
        assert_eq!(row.id, id);
        assert!(d.get_by_cef_id(123).unwrap().is_none());
    }
}
