# buffr-permissions

SQLite-backed per-origin permissions store for buffr. Powers the prompt UI that
asks "https://example.com wants: camera, microphone — allow?" and remembers the
answer when the user picks "always".

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
- `Other(u32)` — single-bit fallback for capabilities buffr does not yet surface
  as a named variant.

`Decision` is two-valued: `Allow` or `Deny`. There is no third "ask every time"
state in the store; an absent row means "ask the user". To reset a remembered
decision call `forget` or `forget_origin`.

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

Migrations are forward-only; adding new capability variants does not require a
schema bump because the storage key is open-ended.

## Decision precedence

The CEF `PermissionHandler` walks this order on every request:

1. **Stored `Decision::Allow`** for every requested capability → callback fires
   synchronously with `Accept` / `cont(mask)`.
2. **Stored `Decision::Deny`** for any requested capability → callback fires
   synchronously with `Deny` / `cancel()`.
3. **Mixed or partially-unknown** → enqueue for the UI thread to prompt. The
   default for an unseen capability is therefore "prompt".

When the user resolves a queued prompt:

- `[a]` allow once → `Accept`, no row written.
- `[A]` allow always → `Accept`, one `Allow` row per capability.
- `[d]`, `[n]` deny once → `Deny` / `cancel()`, no row written.
- `[D]`, `[N]`, `[s]` deny always → `Deny` / `cancel()`, one `Deny` row per
  capability.
- `[Esc]` defer → `Dismiss` / `cancel()`, nothing persisted. The next navigation
  may re-trigger the request.

## CEF callback semantics

A CEF permission callback **must** be invoked exactly once. Dropping the wrapper
without calling `cont()` / `cancel()` leaks a refcounted C++ object and wedges
the renderer until the browser is torn down.

Two safeguards in this crate's caller (`buffr_core::permissions`):

- `PendingPermission::resolve` consumes `self`, so it is impossible to resolve a
  pending request twice through the safe API.
- `drain_with_defer` is invoked at shutdown so any request still queued when the
  user quits gets a `Dismiss` / `cancel()` instead of a leak.

## CLI

`apps/buffr` exposes three short-circuit flags for inspecting and resetting the
store without launching CEF:

```sh
buffr --list-permissions
buffr --clear-permissions
buffr --forget-origin https://example.com
```

## Storage location

`<data>/permissions.sqlite`, where `<data>` is the `directories::ProjectDirs`
data dir for `sh.kryptic.buffr` (`~/.local/share/buffr/` on Linux). Private mode
opens an in-memory DB and discards it at process exit.
