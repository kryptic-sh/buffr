# buffr-zoom

SQLite-backed per-site zoom-level store for buffr.

[![CI](https://github.com/kryptic-sh/buffr/actions/workflows/ci.yml/badge.svg)](https://github.com/kryptic-sh/buffr/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](../../LICENSE)

Phase 5 data layer. Pure storage — no UI, no IPC. Mirrors `buffr-history`,
`buffr-bookmarks`, and `buffr-downloads` in shape: `Mutex<Connection>`,
forward-only migrations, no FTS5.

## Public API

```rust
use buffr_zoom::ZoomStore;

let store = ZoomStore::open("/path/to/zoom.sqlite")?;
store.set("example.com", 0.5)?;        // CEF zoom level; 0.0 = default
let level = store.get("example.com")?; // 0.0 if unset
let _ = store.remove("example.com")?;  // returns bool
let pairs = store.all()?;              // sorted set_at DESC
let n = store.clear()?;                // wipe everything
```

`domain_for_url(url)` canonicalises a URL to `host[:port]` (lowercased). URLs
without a host (`about:`, `data:`, `file:`, `javascript:`, parse errors)
collapse to `_global_`.

## Schema (v1)

```sql
CREATE TABLE zoom (
  domain TEXT PRIMARY KEY,
  level  REAL NOT NULL,
  set_at INTEGER NOT NULL
);
CREATE INDEX idx_zoom_set_at ON zoom(set_at DESC);
```

CEF uses `1.2^level` as the actual scale factor. `0.0` is page default; `1` step
≈ 120%, `-1` step ≈ 83%. The statusline displays the value as a percentage.

## Wiring (apps/buffr)

1. Open `ZoomStore` at `<data>/zoom.sqlite` (in-memory in `--private` mode).
2. Pass `Arc<ZoomStore>` into `BrowserHost::new`. The CEF
   `LoadHandler::on_load_end` looks up the domain level and calls
   `host.set_zoom_level(level)` if non-zero.
3. `PageAction::ZoomIn` / `ZoomOut` adjusts the live level and persists via
   `ZoomStore::set`.
4. `PageAction::ZoomReset` calls `set_zoom_level(0.0)` and
   `ZoomStore::remove(domain)`.

## CLI

```bash
buffr --list-zoom    # print every (domain, level) pair
buffr --clear-zoom   # wipe the store; print count removed
```

Both flags short-circuit before CEF init.

## Testing

`cargo test -p buffr-zoom` — ten unit tests cover open, set/get round-trip,
get-unknown, double-set, remove-existing/missing, ordering, clear, host
extraction, hostless fallback, and the zero-level edge case.

## Storage location

Linux: `~/.local/share/buffr/zoom.sqlite`. macOS / Windows follow
`directories::ProjectDirs` for `sh.kryptic.buffr` — see
[`docs/dev.md`](../../docs/dev.md).

## License

MIT. See [LICENSE](../../LICENSE).
