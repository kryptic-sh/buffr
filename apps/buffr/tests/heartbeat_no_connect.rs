//! Integration test: fake child never connects to the heartbeat socket.
//! Supervisor must time out after 5 s and restart (3× → halt non-zero).
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use tempfile::tempdir;

fn supervisor_bin() -> std::path::PathBuf {
    let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p.push("target");
    p.push("debug");
    p.push("buffr-supervisor");
    p
}

/// A child that sees BUFFR_SUPERVISOR_SOCK in its env but never connects.
/// After 5 s the supervisor kills it (connect-grace timeout) and restarts.
/// Three such failures → backoff halt.
fn no_connect_script(dir: &std::path::Path) -> std::path::PathBuf {
    let script = dir.join("fake-buffr-no-connect");
    // Just sleep without touching the socket.
    let content = "#!/bin/sh\nsleep 30\n";
    fs::write(&script, content).expect("write script");
    let mut perms = fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script, perms).unwrap();
    script
}

#[test]
fn no_connect_triggers_crash_restart_then_halt() {
    let dir = tempdir().expect("tempdir");
    let script = no_connect_script(dir.path());
    let bin = supervisor_bin();

    // Each cycle: 5 s connect-grace + 250 ms cooldown. 3 cycles ≈ 16 s.
    let output = Command::new(&bin)
        .env("BUFFR_CHILD_BIN", &script)
        .env("RUST_LOG", "info")
        // Short timeout so the test stays manageable.
        .arg("--heartbeat-timeout")
        .arg("2")
        .output()
        .expect("failed to run buffr-supervisor");

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
