# buffr — update channel

A version-check + manual-update flow. **No automatic binary replacement.**
Real auto-update needs signing infrastructure (Apple Developer ID +
notarization on macOS, Authenticode on Windows, a signing service we don't
have yet) so it's deferred to post-1.0. What ships today:

1. Once per `[updates] check_interval_hours` (default 24 h), buffr makes
   **one** HTTP GET against the GitHub releases API:
   `https://api.github.com/repos/{repo}/releases/latest`.
2. The result is cached at `<data>/update-cache.json`.
3. The statusline reads the cache on launch; if a newer release exists it
   shows `* upd`. If the cache is older than `check_interval_hours` it shows
   `* upd?` (stale — we don't know if it's still current).
4. The user runs `buffr --check-for-updates` to refresh manually. There is
   no in-chrome "update now" button (no signed binary swap to trigger).

## CLI

```sh
buffr --check-for-updates    # hits the network, prints status, exits 0
buffr --update-status        # reads cache, prints status, exits 0
```

Output format:

```
up-to-date     <current_version>
available      <current>  <latest>  <tag>  <html_url>
stale          <last_checked_rfc3339>  <latest>  <tag>  <html_url>
disabled
error          <message>
```

## Config

```toml
[updates]
# Master switch. When false, buffr makes ZERO network calls — the
# `--check-for-updates` flag short-circuits to "disabled" without
# touching the network. The statusline indicator never appears.
enabled = true

# Reserved for the post-1.0 nightly tag stream. Today only `stable`
# resolves cleanly.
channel = "stable"

# How often `--check-for-updates` is allowed to actually hit GitHub.
# Reads inside the window are served from the disk cache. Minimum 1.
check_interval_hours = 24

# `owner/repo` slug. Forks point this at their own repo.
github_repo = "kryptic-sh/buffr"
```

## What gets sent

A single GET to a public REST endpoint. The request carries no PII:

- Path: `/repos/{repo}/releases/latest`
- Headers: `User-Agent: buffr/<version>` (mandatory — GitHub rejects
  user-agent-less requests) and `Accept: application/vnd.github+json`.
- No cookies, no auth token, no telemetry payload.

GitHub logs the request like any other API request (IP + timestamp). buffr
does **not** receive that log; we do not run our own collector.

## Dismissing a release

`UpdateChecker::dismiss(version)` records a release in the cache as
"ignored". Subsequent `check_cached`/`check_now` for the same version
resolve to `UpToDate` instead of `Available`. Filtering happens at **read
time**, not write time: the cache stays the source of truth for "what
GitHub last reported". A future `--reset-update-dismissals` flag (TODO)
will clear the dismiss list.

## Implementation

- `crates/buffr-core/src/updates.rs` — `UpdateChecker`, `UpdateStatus`,
  `HttpClient` trait, `UreqClient` impl.
- `crates/buffr-config/src/lib.rs` — `[updates]` section schema +
  validation (channel allow-list, repo shape, non-zero interval).
- `apps/buffr/src/main.rs` — `--check-for-updates` / `--update-status` CLI
  short-circuits. Statusline `* upd` indicator wired to the cache read.

The trait `HttpClient` exists so unit tests can drive the state machine
without touching the real network. The real network path uses `ureq` 2.x
with a 5 s connect + 5 s read timeout.
