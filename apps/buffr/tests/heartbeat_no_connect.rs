//! Integration test: fake child never connects to the heartbeat socket.
//! Supervisor must time out (CONNECT_GRACE) and restart (3× → halt non-zero).
//! Forces `BUFFR_CONNECT_GRACE_MS=1000` so the test stays bounded irrespective
//! of the production default (currently 20 s, sized for cold-disk first runs).
#![cfg(unix)]

use std::process::Command;
use tempfile::tempdir;

mod common;
use common::{supervisor_bin, write_script};

/// A child that sees BUFFR_SUPERVISOR_SOCK in its env but never connects.
/// After the grace window the supervisor kills it and restarts.
/// Three such failures → backoff halt.
fn no_connect_script(dir: &std::path::Path) -> std::path::PathBuf {
    // Just sleep without touching the socket.
    write_script(dir, "fake-buffr-no-connect", "#!/bin/sh\nsleep 30\n")
}

#[test]
fn no_connect_triggers_crash_restart_then_halt() {
    let dir = tempdir().expect("tempdir");
    let script = no_connect_script(dir.path());
    let bin = supervisor_bin();

    // Each cycle: 1 s connect-grace (overridden) + 250 ms cooldown. 3 cycles ≈ 4 s.
    let output = Command::new(&bin)
        .env("BUFFR_CHILD_BIN", &script)
        .env("BUFFR_CONNECT_GRACE_MS", "1000")
        .env("RUST_LOG", "info")
        // Short timeout so the test stays manageable.
        .arg("--heartbeat-timeout")
        .arg("2")
        .output()
        .expect("failed to run buffr");

    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "supervisor should exit non-zero after 3 no-connect restarts; got: {:?}\nstderr:\n{stderr}",
        output.status
    );

    assert!(
        stderr.contains("did not connect")
            || stderr.contains("no heartbeat")
            || stderr.contains("refusing to restart"),
        "expected connect-timeout or halt message in stderr; got:\n{stderr}"
    );
}
