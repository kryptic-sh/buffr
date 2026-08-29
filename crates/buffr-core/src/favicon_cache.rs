//! SQLite-backed favicon disk cache for buffr.
//!
//! Persists decoded favicon bitmaps keyed by origin so a restored tab (or any
//! navigation to a previously-seen origin) can paint the cached favicon
//! immediately — before CEF fires its asynchronous `download_image` callback.
//!
//! Matches the `buffr-zoom` / `buffr-history` crate patterns:
//! - `Mutex<Connection>` with WAL journal mode.
//! - Forward-only schema migrations via a `schema_version` table.
//! - An `open_in_memory` constructor for tests and private-mode callers.
//!
//! # Schema (v1)
//!
//! One `favicons` table; `origin` is the PRIMARY KEY. BGRA pixels are stored
//! as a raw `BLOB` of `width * height * 4` bytes (four u8 bytes per u32
//! pixel, little-endian). `updated_at` is unix-epoch seconds.

use std::path::Path;
use std::sync::Mutex;
use std::time::SystemTime;

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use thiserror::Error;
use tracing::{debug, trace};

/// Largest favicon edge we are willing to reconstruct from the cache
/// DB. Real favicons top out around 256px; anything larger is a
/// corrupted or externally-written row, and materialising it would
/// allocate megabytes on the UI path. Rows above this are discarded.
const MAX_FAVICON_EDGE: u32 = 1024;

/// Forward-only migrations. Index `i` → schema version `i + 1`.
const MIGRATIONS: &[&str] = &[
    // v1 — initial schema.
    r#"
    CREATE TABLE IF NOT EXISTS favicons (
        origin     TEXT    PRIMARY KEY,
        width      INTEGER NOT NULL,
        height     INTEGER NOT NULL,
        bgra       BLOB    NOT NULL,
        updated_at INTEGER NOT NULL
    );
    "#,
];

/// Error type for [`FaviconCache`].
#[derive(Debug, Error)]
pub enum FaviconCacheError {
    #[error("opening favicon cache database failed")]
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
    #[error("favicon cache query failed")]
    Query {
        #[from]
        source: rusqlite::Error,
    },
    #[error("favicon cache mutex poisoned")]
    Poisoned,
}

/// A cached favicon retrieved from disk.
#[derive(Debug, Clone)]
pub struct CachedFavicon {
    pub width: u32,
    pub height: u32,
    /// BGRA `u32` pixels, packed as `(a << 24) | (r << 16) | (g << 8) | b`.
    /// Length == `width * height`.
    pub pixels: Vec<u32>,
}

/// SQLite-backed favicon bitmap store.
pub struct FaviconCache {
    conn: Mutex<Connection>,
}

impl FaviconCache {
    /// Open or create the SQLite database at `path` and apply any pending
    /// schema migrations.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, FaviconCacheError> {
        let mut conn = Connection::open_with_flags(
            path.as_ref(),
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )
        .map_err(|source| FaviconCacheError::Open { source })?;
        Self::tune(&conn).map_err(|source| FaviconCacheError::Open { source })?;
        Self::migrate(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// In-memory database — for unit tests and private windows.
    pub fn open_in_memory() -> Result<Self, FaviconCacheError> {
        let mut conn =
            Connection::open_in_memory().map_err(|source| FaviconCacheError::Open { source })?;
        Self::tune(&conn).map_err(|source| FaviconCacheError::Open { source })?;
        Self::migrate(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Apply per-connection pragmas (WAL, NORMAL sync, FK enforcement).
    fn tune(conn: &Connection) -> Result<(), rusqlite::Error> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        Ok(())
    }

    /// Run forward-only migrations.
    fn migrate(conn: &mut Connection) -> Result<(), FaviconCacheError> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY);",
        )
        .map_err(|source| FaviconCacheError::Migrate { source, version: 0 })?;

        let current: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_version",
                [],
                |row| row.get(0),
            )
            .map_err(|source| FaviconCacheError::Migrate { source, version: 0 })?;

        for (idx, sql) in MIGRATIONS.iter().enumerate() {
            let version = (idx + 1) as i64;
            if version <= current {
                continue;
            }
            let tx = conn
                .transaction()
                .map_err(|source| FaviconCacheError::Migrate { source, version })?;
            tx.execute_batch(sql)
                .map_err(|source| FaviconCacheError::Migrate { source, version })?;
            tx.execute(
                "INSERT INTO schema_version(version) VALUES (?1)",
                params![version],
            )
            .map_err(|source| FaviconCacheError::Migrate { source, version })?;
            tx.commit()
                .map_err(|source| FaviconCacheError::Migrate { source, version })?;
        }
        Ok(())
    }

    /// Look up the favicon for `origin`. Returns `None` when the origin is not
    /// cached.
    pub fn get(&self, origin: &str) -> Option<CachedFavicon> {
        let conn = self.conn.lock().ok()?;
        let result: Option<(u32, u32, Vec<u8>)> = conn
            .query_row(
                "SELECT width, height, bgra FROM favicons WHERE origin = ?1",
                params![origin],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .ok()
            .flatten();
        let (width, height, bgra) = result?;
        // Reject implausible dimensions before doing any arithmetic or
        // allocation — a corrupted row can carry `u32::MAX` here.
        if width > MAX_FAVICON_EDGE || height > MAX_FAVICON_EDGE {
            debug!(
                origin,
                width, height, "favicon cache: implausible dimensions — discarding"
            );
            return None;
        }
        let pixel_count = (width as usize).saturating_mul(height as usize);
        // `saturating_mul` throughout: a plain `* 4` would panic in
        // debug / wrap in release and defeat this very length check.
        let expected = pixel_count.saturating_mul(4);
        if bgra.len() != expected {
            debug!(
                origin,
                expected,
                actual = bgra.len(),
                "favicon cache: BLOB length mismatch — discarding"
            );
            return None;
        }
        // `bgra.len()` is a verified multiple of 4 (see the length guard
        // above), so `as_chunks`'s remainder is always empty.
        let pixels: Vec<u32> = bgra
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| {
                let b = c[0] as u32;
                let g = c[1] as u32;
                let r = c[2] as u32;
                let a = c[3] as u32;
                (a << 24) | (r << 16) | (g << 8) | b
            })
            .collect();
        Some(CachedFavicon {
            width,
            height,
            pixels,
        })
    }

    /// UPSERT a favicon for `origin`. Overwrites any previously cached entry.
    /// `pixels` must be BGRA `u32` as produced by the CEF display handler
    /// (packed `(a << 24) | (r << 16) | (g << 8) | b`).
    pub fn put(&self, origin: &str, width: u32, height: u32, pixels: &[u32]) {
        let now = unix_now();
        // Convert u32 pixels → raw BGRA bytes.
        let mut bgra: Vec<u8> = Vec::with_capacity(pixels.len() * 4);
        for &px in pixels {
            let b = (px & 0xFF) as u8;
            let g = ((px >> 8) & 0xFF) as u8;
            let r = ((px >> 16) & 0xFF) as u8;
            let a = ((px >> 24) & 0xFF) as u8;
            bgra.extend_from_slice(&[b, g, r, a]);
        }
        let Ok(conn) = self.conn.lock() else {
            return;
        };
        let result = conn.execute(
            "INSERT INTO favicons (origin, width, height, bgra, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT(origin) DO UPDATE SET \
                 width = excluded.width, \
                 height = excluded.height, \
                 bgra = excluded.bgra, \
                 updated_at = excluded.updated_at",
            params![origin, width, height, bgra, now],
        );
        match result {
            Ok(_) => trace!(origin, width, height, "favicon cache: put"),
            Err(err) => debug!(origin, error = %err, "favicon cache: put failed"),
        }
    }
}

/// Wall-clock unix-epoch seconds. Same pattern as `buffr-zoom`.
fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Extract a canonical origin from a URL string.
///
/// Returns `Some("scheme://host[:port]")` for URLs that have a host, where
/// `port` is included only when explicitly present in the URL.
///
/// Returns `None` for:
/// - `about:`, `chrome:`, `chrome-extension:`, `file:`, `data:`,
///   `javascript:`, and any other scheme without a host component.
/// - Unparseable strings.
///
/// # Examples
///
/// ```
/// use buffr_core::origin_of;
/// assert_eq!(origin_of("https://example.com/path"), Some("https://example.com".into()));
/// assert_eq!(origin_of("http://example.com:8080/"), Some("http://example.com:8080".into()));
/// assert_eq!(origin_of("about:blank"), None);
/// assert_eq!(origin_of("data:text/plain,hi"), None);
/// assert_eq!(origin_of("file:///tmp/foo"), None);
/// ```
pub fn origin_of(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url.trim()).ok()?;
    let host = parsed.host_str()?;
    // Reject hostless / non-web schemes explicitly. The `host_str()` check
    // above already filters most of them, but `file:` can have a host on some
    // platforms.
    let scheme = parsed.scheme();
    match scheme {
        "about" | "chrome" | "chrome-extension" | "file" | "data" | "javascript" => return None,
        _ => {}
    }
    let port_suffix = match parsed.port() {
        Some(p) => format!(":{p}"),
        None => String::new(),
    };
    Some(format!("{scheme}://{host}{port_suffix}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── FaviconCache round-trip ─────────────────────────────────────────────

    #[test]
    fn round_trip_put_then_get() {
        let cache = FaviconCache::open_in_memory().unwrap();
        let pixels: Vec<u32> = vec![0xFF_00_FF_00, 0x00_FF_00_FF];
        cache.put("https://example.com", 2, 1, &pixels);
        let got = cache.get("https://example.com").unwrap();
        assert_eq!(got.width, 2);
        assert_eq!(got.height, 1);
        assert_eq!(got.pixels, pixels);
    }

    /// Insert a row straight into the table, bypassing `put`, so tests
    /// can simulate a corrupted / externally-written `favicon-cache.db`.
    fn insert_raw(cache: &FaviconCache, origin: &str, width: u32, height: u32, bgra: &[u8]) {
        let conn = cache.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO favicons(origin, width, height, bgra, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![origin, width, height, bgra, 0i64],
        )
        .unwrap();
    }

    #[test]
    fn get_rejects_overflowing_dimensions_without_panicking() {
        // Regression (M23): `pixel_count * 4` on a saturated
        // `pixel_count` panicked in debug / wrapped in release,
        // defeating the BLOB-length check it was guarding.
        let cache = FaviconCache::open_in_memory().unwrap();
        insert_raw(
            &cache,
            "https://evil.example",
            u32::MAX,
            u32::MAX,
            &[0u8; 4],
        );
        assert!(cache.get("https://evil.example").is_none());
    }

    #[test]
    fn get_rejects_implausible_dimensions() {
        let cache = FaviconCache::open_in_memory().unwrap();
        let w = MAX_FAVICON_EDGE + 1;
        // Length-consistent, so only the dimension guard can reject it.
        insert_raw(
            &cache,
            "https://huge.example",
            w,
            1,
            &vec![0u8; (w as usize) * 4],
        );
        assert!(cache.get("https://huge.example").is_none());
        // The boundary itself is still accepted.
        insert_raw(
            &cache,
            "https://ok.example",
            MAX_FAVICON_EDGE,
            1,
            &vec![0u8; (MAX_FAVICON_EDGE as usize) * 4],
        );
        assert!(cache.get("https://ok.example").is_some());
    }

    #[test]
    fn get_rejects_blob_length_mismatch() {
        let cache = FaviconCache::open_in_memory().unwrap();
        insert_raw(&cache, "https://short.example", 4, 4, &[0u8; 8]);
        assert!(cache.get("https://short.example").is_none());
    }

    #[test]
    fn get_unknown_origin_returns_none() {
        let cache = FaviconCache::open_in_memory().unwrap();
        assert!(cache.get("https://unknown.example").is_none());
    }

    #[test]
    fn upsert_replaces_existing_entry() {
        let cache = FaviconCache::open_in_memory().unwrap();
        let first = vec![0xDEAD_BEEF_u32];
        let second = vec![0xCAFE_BABE_u32];
        cache.put("https://example.com", 1, 1, &first);
        cache.put("https://example.com", 1, 1, &second);
        let got = cache.get("https://example.com").unwrap();
        assert_eq!(got.pixels, second);
    }

    #[test]
    fn put_multiple_origins_independent() {
        let cache = FaviconCache::open_in_memory().unwrap();
        cache.put("https://a.example", 1, 1, &[0x11_22_33_44]);
        cache.put("https://b.example", 1, 1, &[0x55_66_77_88]);
        let a = cache.get("https://a.example").unwrap();
        let b = cache.get("https://b.example").unwrap();
        assert_ne!(a.pixels, b.pixels);
    }

    // ── origin_of ──────────────────────────────────────────────────────────

    #[test]
    fn origin_of_https_no_port() {
        assert_eq!(
            origin_of("https://example.com/path?q=1#frag"),
            Some("https://example.com".into())
        );
    }

    #[test]
    fn origin_of_http_with_port() {
        assert_eq!(
            origin_of("http://example.com:8080/"),
            Some("http://example.com:8080".into())
        );
    }

    #[test]
    fn origin_of_https_with_port() {
        assert_eq!(
            origin_of("https://sub.example.com:4443/foo"),
            Some("https://sub.example.com:4443".into())
        );
    }

    #[test]
    fn origin_of_rejects_about_blank() {
        assert!(origin_of("about:blank").is_none());
    }

    #[test]
    fn origin_of_rejects_data_url() {
        assert!(origin_of("data:text/html,<h1>hi</h1>").is_none());
    }

    #[test]
    fn origin_of_rejects_file_url() {
        assert!(origin_of("file:///home/user/index.html").is_none());
    }

    #[test]
    fn origin_of_rejects_javascript() {
        assert!(origin_of("javascript:void(0)").is_none());
    }

    #[test]
    fn origin_of_rejects_chrome_scheme() {
        assert!(origin_of("chrome://settings/").is_none());
    }

    #[test]
    fn origin_of_rejects_chrome_extension() {
        assert!(origin_of("chrome-extension://abc123/page.html").is_none());
    }

    #[test]
    fn origin_of_rejects_unparseable() {
        assert!(origin_of("not a url at all").is_none());
        assert!(origin_of("").is_none());
    }
}
