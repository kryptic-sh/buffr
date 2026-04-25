//! SQLite-backed bookmarks store for buffr (Phase 5).
//!
//! Phase-5 scope: a pure data layer. No UI, no IPC. Mirrors the
//! [`buffr_history`] crate's shape — one `Mutex<Connection>`, forward-
//! only migrations, no FTS5 yet.
//!
//! # Schema (v1)
//!
//! Two tables: `bookmarks` (one row per canonical URL) and
//! `bookmark_tags` (many-to-many join). See [`schema`].
//!
//! # Behaviour
//!
//! - URLs are canonicalised through `url::Url::parse`. Failed parse →
//!   [`BookmarkError::Url`].
//! - [`Bookmarks::add`] is **upsert by URL**: if the URL already exists
//!   the title / tags / `modified` get overwritten, no error.
//! - Tags are normalised on the way in — lowercase, trimmed, deduped,
//!   empty entries dropped. Stored lowercase so `by_tag` is a plain
//!   equality lookup.
//! - [`Bookmarks::search`] does case-insensitive substring match over
//!   url, title, and any tag, with ordering
//!   `title-match > url-match > tag-match`, then `modified DESC`.
//! - [`Bookmarks::import_netscape`] parses the Netscape Bookmark File
//!   Format (Chrome/Firefox/Edge export shape) via a regex walker —
//!   the format is loose enough that a real HTML parser is overkill.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use regex::Regex;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, trace};

pub mod schema;

/// Strongly-typed bookmark id. New-type around `i64` so callers can't
/// accidentally pass a history id or a tab id where a bookmark id is
/// expected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BookmarkId(pub i64);

/// One bookmark, decoded into Rust types. Tags are sorted alpha so
/// equality checks in tests don't depend on insertion order.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Bookmark {
    pub id: BookmarkId,
    pub url: String,
    pub title: Option<String>,
    pub tags: Vec<String>,
    pub added: DateTime<Utc>,
    pub modified: DateTime<Utc>,
}

/// Errors surfaced from [`Bookmarks`]. `rusqlite::Error` is wrapped via
/// `#[from]` so callers don't need to depend on `rusqlite` directly.
#[derive(Debug, Error)]
pub enum BookmarkError {
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
    #[error("invalid url")]
    Url {
        #[from]
        source: url::ParseError,
    },
    #[error("bookmarks mutex poisoned")]
    Poisoned,
}

/// SQLite-backed bookmarks store.
pub struct Bookmarks {
    conn: Mutex<Connection>,
}

impl Bookmarks {
    /// Open or create the SQLite database at `path` and run any
    /// pending schema migrations.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, BookmarkError> {
        let mut conn = Connection::open_with_flags(
            path.as_ref(),
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )
        .map_err(|source| BookmarkError::Open { source })?;
        Self::tune(&conn)?;
        schema::apply(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// In-memory database — for tests and short-lived ephemeral
    /// profiles (private windows, Phase 5 follow-up).
    pub fn open_in_memory() -> Result<Self, BookmarkError> {
        let mut conn =
            Connection::open_in_memory().map_err(|source| BookmarkError::Open { source })?;
        Self::tune(&conn)?;
        schema::apply(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Apply per-connection pragmas. Same shape as `buffr-history`.
    fn tune(conn: &Connection) -> Result<(), BookmarkError> {
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|source| BookmarkError::Open { source })?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|source| BookmarkError::Open { source })?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|source| BookmarkError::Open { source })?;
        Ok(())
    }

    /// Add or update a bookmark by URL.
    ///
    /// **Upsert**: if `url` already canonicalises to an existing row,
    /// that row's `title`, `tags`, and `modified` are overwritten and
    /// the existing id is returned. `added` is preserved across
    /// upserts.
    pub fn add(
        &self,
        url: &str,
        title: Option<&str>,
        tags: &[&str],
    ) -> Result<BookmarkId, BookmarkError> {
        let canon = canonicalise(url)?;
        let normalised_tags = normalise_tags(tags);
        let title_owned = title
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let now = Utc::now().timestamp();

        let mut conn = self.conn.lock().map_err(|_| BookmarkError::Poisoned)?;
        let tx = conn.transaction()?;

        let existing: Option<i64> = tx
            .query_row(
                "SELECT id FROM bookmarks WHERE url = ?1",
                params![canon],
                |row| row.get(0),
            )
            .optional()?;

        let id = if let Some(id) = existing {
            tx.execute(
                "UPDATE bookmarks SET title = ?1, modified = ?2 WHERE id = ?3",
                params![title_owned, now, id],
            )?;
            tx.execute(
                "DELETE FROM bookmark_tags WHERE bookmark_id = ?1",
                params![id],
            )?;
            id
        } else {
            tx.execute(
                "INSERT INTO bookmarks (url, title, added, modified) VALUES (?1, ?2, ?3, ?3)",
                params![canon, title_owned, now],
            )?;
            tx.last_insert_rowid()
        };

        for tag in &normalised_tags {
            tx.execute(
                "INSERT OR IGNORE INTO bookmark_tags (bookmark_id, tag) VALUES (?1, ?2)",
                params![id, tag],
            )?;
        }

        tx.commit()?;
        Ok(BookmarkId(id))
    }

    /// Remove a bookmark by id. Returns `true` iff a row was deleted.
    /// `bookmark_tags` rows are removed via `ON DELETE CASCADE`.
    pub fn remove(&self, id: BookmarkId) -> Result<bool, BookmarkError> {
        let conn = self.conn.lock().map_err(|_| BookmarkError::Poisoned)?;
        let n = conn.execute("DELETE FROM bookmarks WHERE id = ?1", params![id.0])?;
        Ok(n > 0)
    }

    /// Update title and/or tags on an existing bookmark. `None` for
    /// either argument leaves that field untouched. Bumps `modified`
    /// only when something actually changes.
    pub fn update(
        &self,
        id: BookmarkId,
        title: Option<Option<&str>>,
        tags: Option<&[&str]>,
    ) -> Result<bool, BookmarkError> {
        if title.is_none() && tags.is_none() {
            return Ok(false);
        }
        let now = Utc::now().timestamp();
        let mut conn = self.conn.lock().map_err(|_| BookmarkError::Poisoned)?;
        let tx = conn.transaction()?;

        let exists: Option<i64> = tx
            .query_row(
                "SELECT id FROM bookmarks WHERE id = ?1",
                params![id.0],
                |row| row.get(0),
            )
            .optional()?;
        if exists.is_none() {
            return Ok(false);
        }

        if let Some(t) = title {
            let t_owned = t
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            tx.execute(
                "UPDATE bookmarks SET title = ?1, modified = ?2 WHERE id = ?3",
                params![t_owned, now, id.0],
            )?;
        }
        if let Some(new_tags) = tags {
            let normalised = normalise_tags(new_tags);
            tx.execute(
                "DELETE FROM bookmark_tags WHERE bookmark_id = ?1",
                params![id.0],
            )?;
            for tag in &normalised {
                tx.execute(
                    "INSERT OR IGNORE INTO bookmark_tags (bookmark_id, tag) VALUES (?1, ?2)",
                    params![id.0, tag],
                )?;
            }
            tx.execute(
                "UPDATE bookmarks SET modified = ?1 WHERE id = ?2",
                params![now, id.0],
            )?;
        }

        tx.commit()?;
        Ok(true)
    }

    /// Fetch a single bookmark by id.
    pub fn get(&self, id: BookmarkId) -> Result<Option<Bookmark>, BookmarkError> {
        let conn = self.conn.lock().map_err(|_| BookmarkError::Poisoned)?;
        let row: Option<(i64, String, Option<String>, i64, i64)> = conn
            .query_row(
                "SELECT id, url, title, added, modified FROM bookmarks WHERE id = ?1",
                params![id.0],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()?;
        let Some((rid, url, title, added, modified)) = row else {
            return Ok(None);
        };
        let tags = load_tags(&conn, rid)?;
        Ok(Some(Bookmark {
            id: BookmarkId(rid),
            url,
            title,
            tags,
            added: ts_to_dt(added),
            modified: ts_to_dt(modified),
        }))
    }

    /// All bookmarks, most recently modified first.
    pub fn all(&self) -> Result<Vec<Bookmark>, BookmarkError> {
        let conn = self.conn.lock().map_err(|_| BookmarkError::Poisoned)?;
        let mut stmt = conn.prepare(
            "SELECT id, url, title, added, modified FROM bookmarks \
             ORDER BY modified DESC, id DESC",
        )?;
        let rows: Vec<(i64, String, Option<String>, i64, i64)> = stmt
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut out = Vec::with_capacity(rows.len());
        for (id, url, title, added, modified) in rows {
            let tags = load_tags(&conn, id)?;
            out.push(Bookmark {
                id: BookmarkId(id),
                url,
                title,
                tags,
                added: ts_to_dt(added),
                modified: ts_to_dt(modified),
            });
        }
        Ok(out)
    }

    /// Bookmarks tagged with `tag` (case-insensitive — input is
    /// normalised the same way storage is).
    pub fn by_tag(&self, tag: &str) -> Result<Vec<Bookmark>, BookmarkError> {
        let needle = tag.trim().to_lowercase();
        if needle.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn.lock().map_err(|_| BookmarkError::Poisoned)?;
        let mut stmt = conn.prepare(
            "SELECT b.id, b.url, b.title, b.added, b.modified FROM bookmarks b \
             JOIN bookmark_tags t ON t.bookmark_id = b.id \
             WHERE t.tag = ?1 \
             ORDER BY b.modified DESC, b.id DESC",
        )?;
        let rows: Vec<(i64, String, Option<String>, i64, i64)> = stmt
            .query_map(params![needle], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut out = Vec::with_capacity(rows.len());
        for (id, url, title, added, modified) in rows {
            let tags = load_tags(&conn, id)?;
            out.push(Bookmark {
                id: BookmarkId(id),
                url,
                title,
                tags,
                added: ts_to_dt(added),
                modified: ts_to_dt(modified),
            });
        }
        Ok(out)
    }

    /// Case-insensitive substring search across url, title, and tags.
    ///
    /// Ordering: title-match (rank 0) > url-match (rank 1) >
    /// tag-match (rank 2), then `modified DESC`. A bookmark is
    /// returned at most once even if it matches in several fields —
    /// the best (lowest-rank) match wins.
    pub fn search(&self, query: &str) -> Result<Vec<Bookmark>, BookmarkError> {
        let needle = query.trim().to_lowercase();
        if needle.is_empty() {
            return self.all();
        }
        let all = self.all()?;
        let mut scored: Vec<(u8, i64, Bookmark)> = Vec::new();
        for bm in all {
            let title_l = bm.title.as_deref().unwrap_or("").to_lowercase();
            let url_l = bm.url.to_lowercase();
            let rank = if title_l.contains(&needle) {
                Some(0u8)
            } else if url_l.contains(&needle) {
                Some(1u8)
            } else if bm.tags.iter().any(|t| t.contains(&needle)) {
                Some(2u8)
            } else {
                None
            };
            if let Some(r) = rank {
                scored.push((r, bm.modified.timestamp(), bm));
            }
        }
        // Sort by (rank ASC, modified DESC). Stable sort, so equal-rank
        // entries already sorted by modified DESC from `all()` keep
        // that ordering — explicit sort just makes the contract loud.
        scored.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
        Ok(scored.into_iter().map(|(_, _, b)| b).collect())
    }

    /// All distinct tags, sorted alphabetically.
    pub fn all_tags(&self) -> Result<Vec<String>, BookmarkError> {
        let conn = self.conn.lock().map_err(|_| BookmarkError::Poisoned)?;
        let mut stmt = conn.prepare("SELECT DISTINCT tag FROM bookmark_tags ORDER BY tag ASC")?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Total bookmark count. Used by tests + diagnostics.
    pub fn count(&self) -> Result<usize, BookmarkError> {
        let conn = self.conn.lock().map_err(|_| BookmarkError::Poisoned)?;
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM bookmarks", [], |row| row.get(0))?;
        Ok(n as usize)
    }

    /// Parse a Netscape Bookmark File Format HTML document and import
    /// every `<A HREF="...">` it finds. Returns the count of
    /// bookmarks successfully inserted (or upserted).
    ///
    /// Folder names from enclosing `<H3>` tags are added as tags on
    /// every `<A>` inside that folder. The `TAGS=` attribute on each
    /// `<A>` (Chrome / Pinboard convention) is split on comma and
    /// merged.
    ///
    /// Implementation: regex-based walker. The Netscape format is
    /// loose HTML — unbalanced tags, no DTD, no closing `</DT>` — so a
    /// strict parser is overkill. We scan top-to-bottom, push folder
    /// names from `<H3>` onto a stack (popping on `</DL>`), and emit
    /// one `add()` per `<A>`.
    pub fn import_netscape(&self, html: &str) -> Result<usize, BookmarkError> {
        // Tokens we walk in document order. We can't reuse a single
        // regex because `regex` doesn't support overlapping captures,
        // and we genuinely need to know the relative ordering of
        // `<H3>`, `</DL>`, and `<A>` to maintain the folder stack.
        let h3_re = Regex::new(r"(?is)<H3[^>]*>(.*?)</H3>").expect("h3 regex");
        let a_re = Regex::new(r#"(?is)<A\s+([^>]*)>(.*?)</A>"#).expect("a regex");
        let dl_close_re = Regex::new(r"(?i)</DL>").expect("dl regex");
        let attr_re = Regex::new(r#"(?i)(\w+)\s*=\s*"([^"]*)""#).expect("attr regex");

        // Collect every match with its byte position so we can sort
        // them into one ordered token stream.
        enum Tok<'a> {
            FolderOpen(&'a str),
            FolderClose,
            Anchor { attrs: &'a str, label: &'a str },
        }
        let mut toks: Vec<(usize, Tok<'_>)> = Vec::new();
        for m in h3_re.captures_iter(html) {
            let pos = m.get(0).map(|m| m.start()).unwrap_or(0);
            let label = m.get(1).map(|m| m.as_str()).unwrap_or("");
            toks.push((pos, Tok::FolderOpen(label)));
        }
        for m in dl_close_re.find_iter(html) {
            toks.push((m.start(), Tok::FolderClose));
        }
        for m in a_re.captures_iter(html) {
            let pos = m.get(0).map(|m| m.start()).unwrap_or(0);
            let attrs = m.get(1).map(|m| m.as_str()).unwrap_or("");
            let label = m.get(2).map(|m| m.as_str()).unwrap_or("");
            toks.push((pos, Tok::Anchor { attrs, label }));
        }
        toks.sort_by_key(|(pos, _)| *pos);

        // Walk: folder stack mirrors `<DL>` nesting via `<H3>` opens
        // and `</DL>` closes. The Netscape format places one `<H3>`
        // immediately before its `<DL>`, and we never see the `<DL>`
        // open token here (we don't need it — the `<H3>` itself opens
        // the folder for tag-purposes).
        let mut folder_stack: Vec<String> = Vec::new();
        let mut count = 0usize;
        for (_, tok) in toks {
            match tok {
                Tok::FolderOpen(label) => {
                    let cleaned = strip_html(label).trim().to_string();
                    folder_stack.push(cleaned);
                }
                Tok::FolderClose => {
                    folder_stack.pop();
                }
                Tok::Anchor { attrs, label } => {
                    let mut href: Option<String> = None;
                    let mut tags: Vec<String> = Vec::new();
                    for m in attr_re.captures_iter(attrs) {
                        let key = m
                            .get(1)
                            .map(|x| x.as_str())
                            .unwrap_or("")
                            .to_ascii_uppercase();
                        let val = m.get(2).map(|x| x.as_str()).unwrap_or("");
                        match key.as_str() {
                            "HREF" => href = Some(val.to_string()),
                            "TAGS" => {
                                for t in val.split(',') {
                                    let trimmed = t.trim();
                                    if !trimmed.is_empty() {
                                        tags.push(trimmed.to_string());
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    let Some(href) = href else {
                        trace!("netscape import: <A> without HREF; skipping");
                        continue;
                    };
                    // Folders → tags. Filter empties so `<H3></H3>`
                    // doesn't poison the tag list.
                    for folder in &folder_stack {
                        if !folder.is_empty() {
                            tags.push(folder.clone());
                        }
                    }
                    let title = strip_html(label);
                    let title_opt = if title.trim().is_empty() {
                        None
                    } else {
                        Some(title)
                    };
                    let tag_refs: Vec<&str> = tags.iter().map(String::as_str).collect();
                    match self.add(&href, title_opt.as_deref(), &tag_refs) {
                        Ok(_) => count += 1,
                        Err(e) => {
                            debug!(error = %e, href = %href, "netscape import: skipping malformed entry");
                        }
                    }
                }
            }
        }
        Ok(count)
    }
}

fn load_tags(conn: &Connection, bookmark_id: i64) -> Result<Vec<String>, BookmarkError> {
    let mut stmt =
        conn.prepare("SELECT tag FROM bookmark_tags WHERE bookmark_id = ?1 ORDER BY tag ASC")?;
    let rows = stmt
        .query_map(params![bookmark_id], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn ts_to_dt(secs: i64) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp(secs, 0)
        .unwrap_or_else(|| DateTime::<Utc>::from_timestamp(0, 0).expect("epoch"))
}

/// Parse + canonicalise a URL string.
fn canonicalise(input: &str) -> Result<String, BookmarkError> {
    let trimmed = input.trim();
    let parsed = url::Url::parse(trimmed)?;
    Ok(parsed.to_string())
}

/// Lowercase, trim, dedupe; drop empties.
fn normalise_tags(tags: &[&str]) -> Vec<String> {
    let mut set: BTreeSet<String> = BTreeSet::new();
    for t in tags {
        let cleaned = t.trim().to_lowercase();
        if !cleaned.is_empty() {
            set.insert(cleaned);
        }
    }
    set.into_iter().collect()
}

/// Strip a small set of HTML tags from a string. Netscape titles
/// occasionally contain `<B>`, `<I>`, `<BR>`. We don't need a real
/// HTML parser — just drop the angle-bracketed bits.
fn strip_html(s: &str) -> String {
    // SAFETY: regex is a constant; compile error only on programmer
    // mistake, which would surface in tests.
    let re = Regex::new(r"<[^>]+>").expect("strip_html regex");
    re.replace_all(s, "").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_in_memory_runs_migrations() {
        let b = Bookmarks::open_in_memory().unwrap();
        assert_eq!(b.count().unwrap(), 0);
        assert_eq!(schema::latest_version(), 1);
    }

    #[test]
    fn add_three_then_all_orders_most_recent_first() {
        let b = Bookmarks::open_in_memory().unwrap();
        b.add("https://a.example/", Some("A"), &["foo"]).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        b.add("https://b.example/", Some("B"), &["foo"]).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        b.add("https://c.example/", Some("C"), &["foo"]).unwrap();

        assert_eq!(b.count().unwrap(), 3);
        let all = b.all().unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].url, "https://c.example/");
        assert_eq!(all[1].url, "https://b.example/");
        assert_eq!(all[2].url, "https://a.example/");
    }

    #[test]
    fn add_same_url_twice_upserts() {
        let b = Bookmarks::open_in_memory().unwrap();
        let id1 = b.add("https://a.example/", Some("First"), &["t1"]).unwrap();
        let id2 = b
            .add("https://a.example/", Some("Second"), &["t2"])
            .unwrap();
        assert_eq!(id1, id2);
        assert_eq!(b.count().unwrap(), 1);
        let bm = b.get(id1).unwrap().expect("exists");
        assert_eq!(bm.title.as_deref(), Some("Second"));
        assert_eq!(bm.tags, vec!["t2"]);
    }

    #[test]
    fn tags_normalised_lowercase_trimmed_deduped_empty_rejected() {
        let b = Bookmarks::open_in_memory().unwrap();
        let id = b
            .add(
                "https://a.example/",
                None,
                &["RUST", "  rust  ", "rust", ""],
            )
            .unwrap();
        let bm = b.get(id).unwrap().expect("exists");
        assert_eq!(bm.tags, vec!["rust"]);
    }

    #[test]
    fn by_tag_filters() {
        let b = Bookmarks::open_in_memory().unwrap();
        b.add("https://a.example/", Some("A"), &["rust", "lang"])
            .unwrap();
        b.add("https://b.example/", Some("B"), &["python"]).unwrap();
        b.add("https://c.example/", Some("C"), &["rust"]).unwrap();

        let rust_hits = b.by_tag("rust").unwrap();
        assert_eq!(rust_hits.len(), 2);
        let urls: Vec<&str> = rust_hits.iter().map(|x| x.url.as_str()).collect();
        assert!(urls.contains(&"https://a.example/"));
        assert!(urls.contains(&"https://c.example/"));
        // Case-insensitive.
        assert_eq!(b.by_tag("RUST").unwrap().len(), 2);
    }

    #[test]
    fn search_orders_title_url_tag() {
        let b = Bookmarks::open_in_memory().unwrap();
        // Tag-only match.
        b.add("https://other.test/", Some("Other"), &["foobar"])
            .unwrap();
        // URL-only match.
        b.add("https://foobar.example/", Some("Unrelated"), &["nope"])
            .unwrap();
        // Title match.
        b.add("https://x.test/", Some("Foobar Frenzy"), &["nope"])
            .unwrap();

        let hits = b.search("foobar").unwrap();
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].title.as_deref(), Some("Foobar Frenzy"));
        assert_eq!(hits[1].url, "https://foobar.example/");
        assert_eq!(hits[2].url, "https://other.test/");
    }

    #[test]
    fn all_tags_sorted_unique() {
        let b = Bookmarks::open_in_memory().unwrap();
        b.add("https://a.example/", None, &["zeta", "alpha"])
            .unwrap();
        b.add("https://b.example/", None, &["alpha", "mid"])
            .unwrap();
        let tags = b.all_tags().unwrap();
        assert_eq!(tags, vec!["alpha", "mid", "zeta"]);
    }

    #[test]
    fn update_then_get_round_trip() {
        let b = Bookmarks::open_in_memory().unwrap();
        let id = b.add("https://a.example/", Some("Old"), &["t1"]).unwrap();
        let changed = b
            .update(id, Some(Some("New")), Some(&["t2", "t3"]))
            .unwrap();
        assert!(changed);
        let bm = b.get(id).unwrap().expect("exists");
        assert_eq!(bm.title.as_deref(), Some("New"));
        assert_eq!(bm.tags, vec!["t2", "t3"]);
    }

    #[test]
    fn remove_returns_true_then_false() {
        let b = Bookmarks::open_in_memory().unwrap();
        let id = b.add("https://a.example/", None, &[]).unwrap();
        assert!(b.remove(id).unwrap());
        assert!(!b.remove(id).unwrap());
        assert_eq!(b.count().unwrap(), 0);
    }

    #[test]
    fn invalid_url_errors() {
        let b = Bookmarks::open_in_memory().unwrap();
        let err = b.add("not a url", None, &[]);
        assert!(matches!(err, Err(BookmarkError::Url { .. })));
    }

    const NETSCAPE_FIXTURE: &str = r#"<!DOCTYPE NETSCAPE-Bookmark-file-1>
<META HTTP-EQUIV="Content-Type" CONTENT="text/html; charset=UTF-8">
<TITLE>Bookmarks</TITLE>
<H1>Bookmarks</H1>
<DL><p>
    <DT><H3>Rust</H3>
    <DL><p>
        <DT><A HREF="https://rust-lang.org/" ADD_DATE="1700000000">Rust language</A>
        <DT><A HREF="https://crates.io/" ADD_DATE="1700000001" TAGS="package,registry">crates.io</A>
    </DL><p>
    <DT><H3>News</H3>
    <DL><p>
        <DT><A HREF="https://news.example.com/a" ADD_DATE="1700000002">A</A>
        <DT><A HREF="https://news.example.com/b" ADD_DATE="1700000003">B</A>
        <DT><A HREF="https://news.example.com/c" ADD_DATE="1700000004">C</A>
    </DL><p>
</DL><p>
"#;

    #[test]
    fn import_netscape_5_bookmarks_2_folders() {
        let b = Bookmarks::open_in_memory().unwrap();
        let imported = b.import_netscape(NETSCAPE_FIXTURE).unwrap();
        assert_eq!(imported, 5);
        assert_eq!(b.count().unwrap(), 5);

        let rust_hits = b.by_tag("rust").unwrap();
        assert_eq!(rust_hits.len(), 2);
        let news_hits = b.by_tag("news").unwrap();
        assert_eq!(news_hits.len(), 3);

        // TAGS= attribute also imported.
        let pkg = b.by_tag("package").unwrap();
        assert_eq!(pkg.len(), 1);
        assert_eq!(pkg[0].url, "https://crates.io/");
    }

    #[test]
    fn import_netscape_skips_malformed() {
        let b = Bookmarks::open_in_memory().unwrap();
        let html = r#"<DL>
            <DT><A HREF="https://ok.example/">Good</A>
            <DT><A HREF="not a url">Bad</A>
            <DT><A>NoHref</A>
        </DL>"#;
        let imported = b.import_netscape(html).unwrap();
        assert_eq!(imported, 1);
        assert_eq!(b.count().unwrap(), 1);
    }
}
