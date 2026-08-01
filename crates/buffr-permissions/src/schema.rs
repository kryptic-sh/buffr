//! SQLite schema + forward-only migrations for [`crate::Permissions`].
//!
//! Same `schema_version` table pattern as the other buffr stores: one
//! row per applied migration, monotonically increasing. Append new
//! migrations to [`MIGRATIONS`]; never rewrite an old entry.

use rusqlite::Connection;

use crate::PermError;

/// Forward-only migrations. Index `i` here corresponds to schema
/// version `i + 1`.
const MIGRATIONS: &[&str] = &[
    // v1 — initial schema. One row per (origin, capability); `decision`
    // is a serde-rendered `snake_case` enum string ("allow" / "deny");
    // `set_at` is unix-epoch seconds, used for `all()` ordering.
    r#"
    CREATE TABLE IF NOT EXISTS permissions (
      origin     TEXT NOT NULL,
      capability TEXT NOT NULL,
      decision   TEXT NOT NULL,
      set_at     INTEGER NOT NULL,
      PRIMARY KEY (origin, capability)
    );
    CREATE INDEX IF NOT EXISTS idx_permissions_set_at
      ON permissions(set_at DESC);
    "#,
];

/// Run all pending migrations. Thin wrapper over the shared runner in
/// [`buffr_store`] — the only crate-specific part is mapping the
/// failure into [`PermError`].
pub(crate) fn apply(conn: &mut Connection) -> Result<(), PermError> {
    buffr_store::apply(conn, MIGRATIONS).map_err(|e| match e {
        buffr_store::MigrationError::Sql { source, version } => {
            PermError::Migrate { source, version }
        }
        buffr_store::MigrationError::TooNew { found, supported } => {
            PermError::SchemaTooNew { found, supported }
        }
    })
}

/// Highest version the binary knows about. Public for diagnostics.
pub fn latest_version() -> i64 {
    buffr_store::latest_version(MIGRATIONS)
}
