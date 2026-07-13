# Buffr Robustness Fixes — Items Needing User Input

This file tracks issues found during robustness audit that require user
input before implementing changes.

## 1. `set_permissions` with `from_mode` (stable vs unstable)

**File:** `apps/buffr/src/main.rs:502`
```rust
std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
```

`std::fs::Permissions::from_mode()` is a Unix-specific extension from
`std::os::unix::fs::PermissionsExt` trait, imported at line 169. This
IS the stable API — confirmed.

**Status:** ✅ No action needed — stable.

## 2. Signal forwarding — `nix::sys::signal::kill` with `None` signal

**File:** `apps/buffr/src/main.rs:765`
```rust
if matches!(signal::kill(child_pid, None), Err(nix::errno::Errno::ESRCH)) {
```

This uses `None` to mean "don't send a signal, just check if process exists".
The nix crate's `kill` function accepts `Option<Signal>` where `None` performs
a 0-signal existence check. This is correct.

**Status:** ✅ No action needed — correct.

## 3. `getrandom::getrandom().expect()` in internal server

**File:** `crates/buffr-engine/src/internal_server.rs:367`
```rust
getrandom::getrandom(&mut buf).expect("OS CSPRNG unavailable");
```

The comment explains this is intentional: a system without `/dev/urandom`
is broken beyond recovery. However, on some minimal containers/CI the
`getrandom` syscall can fail. Could fall back to a mixed entropy pool.

**Status:** ❓ **Needs user input** — is a graceful fallback desired here,
or should this remain a panic?

## 4. `now_secs()` returning epoch on system-time-before-epoch

**File:** `apps/buffr-app/src/crash_guard.rs:118-123`
```rust
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
```

`unwrap_or(0)` on failure returns year 1970 timestamps. This is an
extreme edge case (system clock before 1970).

**Status:** ✅ Low risk — practically impossible on any real system.

## 5. History/bookmarks/downloads `VACUUM` failures logged at `warn!`

Multiple stores call `VACUUM` after `clear_all()` and log at `warn!` on
failure. This is intentional and documented — the data is already deleted,
VACUUM is just storage hygiene.

**Status:** ✅ Correct as designed.

## 6. Excluded crate code quality (buffr-webkit, buffr-poc)

The `buffr-webkit` and `buffr-poc` crates are excluded from the workspace
and not built by CI. They contain significant `unsafe` blocks. A review
of these crates is deferred — they are experimental and not production code.

**Status:** ❓ **Deferred** — experimental crates, not in workspace.

## 7. Supervisor `BUFFR_CONNECT_GRACE_MS` env var parsing

**File:** `apps/buffr/src/main.rs:194-200`
The env var can be set to extreme values (0, or millions) which could
cause the supervisor to immediately time out or wait forever.

**Status:** ⚠️ Low priority — env var is only for test overrides.
Consider adding lower/upper bounds if this becomes a problem.
