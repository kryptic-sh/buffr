//! SQLite schema + forward-only migrations for [`crate::ZoomStore`].
//!
//! Same `schema_version` table pattern as `buffr-history` /
//! `buffr-bookmarks` / `buffr-downloads`: one row per applied
//! migration, monotonically increasing. Append new migrations to
//! [`MIGRATIONS`]; never rewrite an old entry.

use rusqlite::Connection;

use crate::ZoomError;

/// Forward-only migrations. Index `i` here corresponds to schema
/// version `i + 1`.
const MIGRATIONS: &[&str] = &[
    // v1 — initial schema. One row per `domain` (PRIMARY KEY); `level`
    // is the CEF zoom level (0.0 = default, +0.25 per zoom-in step);
    // `set_at` is unix-epoch seconds, used for `all()` ordering.
    r#"
    CREATE TABLE IF NOT EXISTS zoom (
      domain TEXT PRIMARY KEY,
      level  REAL NOT NULL,
      set_at INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_zoom_set_at ON zoom(set_at DESC);
    "#,
];

/// Run all pending migrations. Thin wrapper over the shared runner in
/// [`buffr_store`] — the only crate-specific part is mapping the
/// failure into [`ZoomError`].
pub(crate) fn apply(conn: &mut Connection) -> Result<(), ZoomError> {
    buffr_store::apply(conn, MIGRATIONS).map_err(|e| match e {
        buffr_store::MigrationError::Sql { source, version } => {
            ZoomError::Migrate { source, version }
        }
        buffr_store::MigrationError::TooNew { found, supported } => {
            ZoomError::SchemaTooNew { found, supported }
        }
    })
}

/// Highest version the binary knows about. Public for diagnostics.
pub fn latest_version() -> i64 {
    buffr_store::latest_version(MIGRATIONS)
}
