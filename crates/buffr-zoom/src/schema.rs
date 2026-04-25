//! SQLite schema + forward-only migrations for [`crate::ZoomStore`].
//!
//! Same `schema_version` table pattern as `buffr-history` /
//! `buffr-bookmarks` / `buffr-downloads`: one row per applied
//! migration, monotonically increasing. Append new migrations to
//! [`MIGRATIONS`]; never rewrite an old entry.

use rusqlite::{Connection, params};

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

/// Run all pending migrations.
pub(crate) fn apply(conn: &mut Connection) -> Result<(), ZoomError> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY);")
        .map_err(|source| ZoomError::Migrate { source, version: 0 })?;

    let current: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |row| row.get(0),
        )
        .map_err(|source| ZoomError::Migrate { source, version: 0 })?;

    for (idx, sql) in MIGRATIONS.iter().enumerate() {
        let version = (idx + 1) as i64;
        if version <= current {
            continue;
        }
        let tx = conn
            .transaction()
            .map_err(|source| ZoomError::Migrate { source, version })?;
        tx.execute_batch(sql)
            .map_err(|source| ZoomError::Migrate { source, version })?;
        tx.execute(
            "INSERT INTO schema_version(version) VALUES (?1)",
            params![version],
        )
        .map_err(|source| ZoomError::Migrate { source, version })?;
        tx.commit()
            .map_err(|source| ZoomError::Migrate { source, version })?;
    }

    Ok(())
}

/// Highest version the binary knows about. Public for diagnostics.
pub fn latest_version() -> i64 {
    MIGRATIONS.len() as i64
}
