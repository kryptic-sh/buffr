//! Integration test: `--heartbeat-disable` flag.
//!
//! When the flag is present:
//!   - No socket is bound (BUFFR_SUPERVISOR_SOCK must NOT be set in child env).
//!   - Round-1 crash-only behaviour is preserved.
#![cfg(unix)]

use std::process::Command;
use tempfile::tempdir;

mod common;
use common::{crasher_script, supervisor_bin, write_script};

/// Write a script that:
///   1. Checks whether BUFFR_SUPERVISOR_SOCK is set.
///   2. If set → exits 2 (test failure signal).
///   3. If unset → exits 0 (correct behaviour under --heartbeat-disable).
fn env_check_script(dir: &std::path::Path) -> std::path::PathBuf {
    let content = r#"#!/bin/sh
if [ -n "$BUFFR_SUPERVISOR_SOCK" ]; then
    echo "FAIL: BUFFR_SUPERVISOR_SOCK is set but should not be" >&2
    exit 2
fi
exit 0
"#;
    write_script(dir, "fake-buffr-env-check", content)
}

#[test]
fn heartbeat_disable_does_not_pass_sock_env_to_child() {
    let dir = tempdir().expect("tempdir");
    let script = env_check_script(dir.path());
    let bin = supervisor_bin();

    let output = Command::new(&bin)
        .env("BUFFR_CHILD_BIN", &script)
        .env("RUST_LOG", "info")
        // A stale value in our own environment must not leak to the child.
        .env("BUFFR_SUPERVISOR_SOCK", "/nonexistent/stale.sock")
        .arg("--heartbeat-disable")
        .output()
        .expect("failed to run buffr");

    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "--heartbeat-disable: child (env checker) should exit 0; got: {:?}\nstderr:\n{stderr}",
        output.status
    );
}

/// With --heartbeat-disable, crash-restart still works (Round-1 behaviour).
///
/// `crasher_script` SIGABRTs itself so the child dies via signal, not via
/// `exit 1`. The supervisor distinguishes the two: normal non-zero exits are
/// treated as CLI / panic errors and propagated without restart, while
/// signal-style deaths still go through the restart-with-backoff path.
#[test]
fn heartbeat_disable_preserves_crash_restart_backoff() {
    let dir = tempdir().expect("tempdir");
    let script = crasher_script(dir.path());
    let bin = supervisor_bin();

    let output = Command::new(&bin)
        .env("BUFFR_CHILD_BIN", &script)
        .env("RUST_LOG", "info")
        .arg("--heartbeat-disable")
        .output()
        .expect("failed to run buffr");

    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "supervisor should exit non-zero after 3 crashes; got: {:?}\nstderr:\n{stderr}",
        output.status
    );
    assert!(
        stderr.contains("refusing to restart") || stderr.contains("crashes"),
        "expected backoff halt message; got:\n{stderr}"
    );
}
