# buffr-zoom

SQLite-backed per-site zoom-level store for buffr.

## Scope

Phase 5 data layer. Pure storage — no UI, no IPC. Mirrors `buffr-history`,
`buffr-bookmarks`, and `buffr-downloads` in shape: `Mutex<Connection>`,
forward-only migrations, no FTS5.

## Schema (v1)

```sql
CREATE TABLE zoom (
  domain TEXT PRIMARY KEY,
  level  REAL NOT NULL,
  set_at INTEGER NOT NULL
);
CREATE INDEX idx_zoom_set_at ON zoom(set_at DESC);
```

`domain` is `host[:port]` (lowercased) for URLs that have a host. URLs without a
host (`about:`, `data:`, `file:`, `javascript:`, parse errors) collapse to a
single sentinel key, `_global_`. Use [`domain_for_url`] to canonicalise.

## API

```rust
let store = ZoomStore::open("/path/to/zoom.sqlite")?;
store.set("example.com", 0.5)?;        // CEF zoom level; 0.0 = default
let level = store.get("example.com")?; // 0.0 if unset
let _ = store.remove("example.com")?;  // returns bool
let pairs = store.all()?;              // sorted set_at DESC
let n = store.clear()?;                // wipe everything
```

## Wiring (apps/buffr)

1. Open `ZoomStore` at `<data>/zoom.sqlite` (in-memory in `--private` mode).
2. Pass `Arc<ZoomStore>` into `BrowserHost::new`. The CEF
   `LoadHandler::on_load_end` reads `domain_for_url(frame.url())`, looks up the
   level, and calls `host.set_zoom_level(level)` if non-zero.
3. `PageAction::ZoomIn` / `ZoomOut` adjusts the live level and persists via
   `ZoomStore::set(domain, new_level)`.
4. `PageAction::ZoomReset` calls `set_zoom_level(0.0)` and
   `ZoomStore::remove(domain)`.

## CLI

- `buffr --list-zoom` — print every `(domain, level)` pair.
- `buffr --clear-zoom` — wipe the store; print the count removed.

Both flags short-circuit before CEF init.

## Storage path

Linux: `~/.local/share/buffr/zoom.sqlite`. macOS / Windows follow
`directories::ProjectDirs` resolution for `sh.kryptic.buffr` — see
`docs/dev.md`.

## Tests

Ten unit tests cover open, set/get round-trip, get-unknown, double-set,
remove-existing/missing, ordering, clear, host extraction, hostless fallback,
and the zero-level edge case. Run with `cargo test -p buffr-zoom`.
