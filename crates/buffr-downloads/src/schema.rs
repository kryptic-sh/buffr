//! SQLite schema + forward-only migrations for [`crate::Downloads`].
//!
//! Same `schema_version` table pattern as `buffr-history` and
//! `buffr-bookmarks`: one row per applied migration, monotonically
//! increasing. Append new migrations to [`MIGRATIONS`]; never rewrite
//! an old entry.

use rusqlite::Connection;

use crate::DownloadError;

/// Forward-only migrations. Index `i` here corresponds to schema
/// version `i + 1`.
const MIGRATIONS: &[&str] = &[
    // v1 — initial schema. Single `downloads` table holding one row
    // per CEF-issued download. `cef_id` is unique because CEF reuses
    // the same id across `OnBeforeDownload` + every `OnDownloadUpdated`
    // tick — `record_started` upserts on it. `status` is a plain TEXT
    // tag (`in_flight` | `completed` | `canceled` | `failed`) so we
    // can extend without a migration. The two indexes cover the
    // common access patterns: status-filtered ordering for the
    // downloads pane, and cef_id lookup for in-flight callbacks.
    r#"
    CREATE TABLE IF NOT EXISTS downloads (
      id              INTEGER PRIMARY KEY AUTOINCREMENT,
      cef_id          INTEGER NOT NULL UNIQUE,
      url             TEXT NOT NULL,
      suggested_name  TEXT NOT NULL,
      mime            TEXT,
      total_bytes     INTEGER,
      received_bytes  INTEGER NOT NULL DEFAULT 0,
      status          TEXT NOT NULL,
      started_at      INTEGER NOT NULL,
      finished_at     INTEGER,
      full_path       TEXT,
      failure         TEXT
    );
    CREATE INDEX IF NOT EXISTS idx_downloads_status ON downloads(status, started_at DESC);
    CREATE INDEX IF NOT EXISTS idx_downloads_cef_id ON downloads(cef_id);
    "#,
];

/// Run all pending migrations. Thin wrapper over the shared runner in
/// [`buffr_store`] — the only crate-specific part is mapping the
/// failure into [`DownloadError`].
pub(crate) fn apply(conn: &mut Connection) -> Result<(), DownloadError> {
    buffr_store::apply(conn, MIGRATIONS).map_err(|e| match e {
        buffr_store::MigrationError::Sql { source, version } => {
            DownloadError::Migrate { source, version }
        }
        buffr_store::MigrationError::TooNew { found, supported } => {
            DownloadError::SchemaTooNew { found, supported }
        }
    })
}

/// Highest version the binary knows about. Public for diagnostics.
pub fn latest_version() -> i64 {
    buffr_store::latest_version(MIGRATIONS)
}
