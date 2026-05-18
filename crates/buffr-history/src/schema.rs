//! SQLite schema + forward-only migrations for [`crate::History`].
//!
//! The `schema_version` table holds a single row (`version INTEGER`)
//! recording the highest migration index applied. On open we read that
//! number, run the migrations in [`MIGRATIONS`] from
//! `current_version + 1` onward, and write the new highest index back
//! in the same transaction. Each migration is a single SQL string; if
//! we ever need multi-statement migrations we'll split per-statement
//! and call `execute_batch`.
//!
//! v1 is the only migration; future migrations append to the array
//! without renumbering. **Never** rewrite history (heh) — append only.

use rusqlite::{Connection, params};

use crate::HistoryError;

/// Forward-only migrations. Index `i` here corresponds to schema
/// version `i + 1` (so v1 is `MIGRATIONS[0]`, v2 is `MIGRATIONS[1]`,
/// etc.). Adding a migration appends to this slice.
const MIGRATIONS: &[&str] = &[
    // v1 — initial schema. Single `visits` table holding one row per
    // visit, plus an explicit transition column so Phase 5 part 2 can
    // weight frecency by transition kind. Indexes cover the two
    // common access patterns (lookup-by-url, recent-first ordering).
    r#"
    CREATE TABLE IF NOT EXISTS visits (
      id            INTEGER PRIMARY KEY AUTOINCREMENT,
      url           TEXT NOT NULL,
      title         TEXT,
      visit_time    INTEGER NOT NULL,
      transition    TEXT NOT NULL DEFAULT 'link'
    );
    CREATE INDEX IF NOT EXISTS idx_visits_url ON visits(url);
    CREATE INDEX IF NOT EXISTS idx_visits_time ON visits(visit_time DESC);
    CREATE INDEX IF NOT EXISTS idx_visits_url_time ON visits(url, visit_time DESC);
    "#,
    // v2 — FTS5 external-content index over (url, title) using the
    // unicode61 tokenizer with diacritic folding. `content='visits'`
    // + `content_rowid='id'` means the FTS shadow tables mirror real
    // rows; MATCH joins land on rowid so the frecency query can JOIN
    // visits_fts ON rowid = visits.id. Three triggers keep the index
    // in sync with INSERT / DELETE / UPDATE on `visits`.
    r#"
    CREATE VIRTUAL TABLE visits_fts USING fts5(
      url,
      title,
      content='visits',
      content_rowid='id',
      tokenize='unicode61 remove_diacritics 2'
    );

    INSERT INTO visits_fts(rowid, url, title)
    SELECT id, url, COALESCE(title, '') FROM visits;

    CREATE TRIGGER visits_ai AFTER INSERT ON visits BEGIN
      INSERT INTO visits_fts(rowid, url, title)
      VALUES (new.id, new.url, COALESCE(new.title, ''));
    END;
    CREATE TRIGGER visits_ad AFTER DELETE ON visits BEGIN
      INSERT INTO visits_fts(visits_fts, rowid, url, title)
      VALUES ('delete', old.id, old.url, COALESCE(old.title, ''));
    END;
    CREATE TRIGGER visits_au AFTER UPDATE ON visits BEGIN
      INSERT INTO visits_fts(visits_fts, rowid, url, title)
      VALUES ('delete', old.id, old.url, COALESCE(old.title, ''));
      INSERT INTO visits_fts(rowid, url, title)
      VALUES (new.id, new.url, COALESCE(new.title, ''));
    END;
    "#,
];

/// Run all pending migrations, leaving `schema_version` reflecting the
/// new high-water mark.
pub(crate) fn apply(conn: &mut Connection) -> Result<(), HistoryError> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY);")
        .map_err(|source| HistoryError::Migrate { source, version: 0 })?;

    let current: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |row| row.get(0),
        )
        .map_err(|source| HistoryError::Migrate { source, version: 0 })?;

    for (idx, sql) in MIGRATIONS.iter().enumerate() {
        let version = (idx + 1) as i64;
        if version <= current {
            continue;
        }
        let tx = conn
            .transaction()
            .map_err(|source| HistoryError::Migrate { source, version })?;
        tx.execute_batch(sql)
            .map_err(|source| HistoryError::Migrate { source, version })?;
        tx.execute(
            "INSERT INTO schema_version(version) VALUES (?1)",
            params![version],
        )
        .map_err(|source| HistoryError::Migrate { source, version })?;
        tx.commit()
            .map_err(|source| HistoryError::Migrate { source, version })?;
    }

    Ok(())
}

/// Highest version the binary knows about. Public for diagnostics.
pub fn latest_version() -> i64 {
    MIGRATIONS.len() as i64
}
