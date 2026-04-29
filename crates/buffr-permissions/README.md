# buffr-permissions

SQLite-backed per-origin permissions store for buffr.

[![CI](https://github.com/kryptic-sh/buffr/actions/workflows/ci.yml/badge.svg)](https://github.com/kryptic-sh/buffr/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](../../LICENSE)

Powers the prompt UI that asks "https://example.com wants: camera, microphone —
allow?" and remembers the answer when the user picks "always". CEF's
`PermissionHandler` consults this store on every permission request;
`apps/buffr` persists user choices through the same handle.

## Public API

```rust
use buffr_permissions::{Capability, Decision, Permissions};

let store = Permissions::open(path)?;
store.set("https://example.com", Capability::Camera, Decision::Allow)?;
let dec = store.get("https://example.com", Capability::Camera)?; // Some(Allow)
store.forget("https://example.com", Capability::Camera)?;        // true
store.forget_origin("https://example.com")?;                     // count
let rows = store.all()?;
let n = store.clear()?;
```

`Capability` mirrors a subset of `cef_permission_request_types_t`:

- `Camera`
- `Microphone`
- `Geolocation`
- `Notifications`
- `Clipboard`
- `Midi`
- `Other(u32)` — single-bit fallback for capabilities not yet surfaced as a
  named variant.

`Decision` is two-valued: `Allow` or `Deny`. An absent row means "ask the user".
To reset a remembered decision call `forget` or `forget_origin`.

## Schema (v1)

```sql
CREATE TABLE IF NOT EXISTS permissions (
  origin     TEXT NOT NULL,
  capability TEXT NOT NULL,
  decision   TEXT NOT NULL,
  set_at     INTEGER NOT NULL,
  PRIMARY KEY (origin, capability)
);
CREATE INDEX IF NOT EXISTS idx_permissions_set_at
  ON permissions(set_at DESC);
```

`capability` is the storage-key string (`camera`, `microphone`, …,
`other:<bit>`). `decision` is `allow` / `deny` (serde snake_case). `set_at` is
unix-epoch seconds.

Migrations are forward-only; adding new capability variants doesn't require a
schema bump because the storage key is open-ended.

## Decision precedence

The CEF `PermissionHandler` walks this order on every request:

1. **Stored `Decision::Allow`** for every requested capability → callback fires
   synchronously with `Accept`.
2. **Stored `Decision::Deny`** for any requested capability → callback fires
   synchronously with `Deny`.
3. **Mixed or partially-unknown** → enqueue for the UI thread to prompt. The
   default for an unseen capability is "prompt".

When the user resolves a queued prompt:

| Key               | Outcome                                                  |
| ----------------- | -------------------------------------------------------- |
| `[a]`             | Allow once — `Accept`, no row written.                   |
| `[A]`             | Allow always — `Accept`, one `Allow` row per capability. |
| `[d]`/`[n]`       | Deny once — `Deny`, no row written.                      |
| `[D]`/`[N]`/`[s]` | Deny always — `Deny`, one `Deny` row per capability.     |
| `[Esc]`           | Defer — `Dismiss`, nothing persisted.                    |

## CEF callback safety

A CEF permission callback **must** be invoked exactly once. Dropping the wrapper
without calling `cont()` / `cancel()` leaks a refcounted C++ object and wedges
the renderer.

Two safeguards in `buffr_core::permissions`:

- `PendingPermission::resolve` consumes `self` — impossible to resolve a request
  twice.
- `drain_with_defer` is invoked at shutdown so any pending request gets a
  `Dismiss`.

## CLI

`apps/buffr` exposes short-circuit flags:

```sh
buffr --list-permissions
buffr --clear-permissions
buffr --forget-origin https://example.com
```

## Storage location

`<data>/permissions.sqlite`; on Linux that's
`~/.local/share/buffr/permissions.sqlite`. Private mode opens an in-memory DB
discarded at process exit.

## License

MIT. See [LICENSE](../../LICENSE).
