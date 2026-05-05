//! Integration test: `--heartbeat-disable` flag.
//!
//! When the flag is present:
//!   - No socket is bound (BUFFR_SUPERVISOR_SOCK must NOT be set in child env).
//!   - Round-1 crash-only behaviour is preserved.
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
    p.push("buffr");
    p
}

/// Write a script that:
///   1. Checks whether BUFFR_SUPERVISOR_SOCK is set.
///   2. If set → exits 2 (test failure signal).
///   3. If unset → exits 0 (correct behaviour under --heartbeat-disable).
fn env_check_script(dir: &std::path::Path) -> std::path::PathBuf {
    let script = dir.join("fake-buffr-env-check");
    let content = r#"#!/bin/sh
if [ -n "$BUFFR_SUPERVISOR_SOCK" ]; then
    echo "FAIL: BUFFR_SUPERVISOR_SOCK is set but should not be" >&2
    exit 2
fi
exit 0
"#;
    fs::write(&script, content).expect("write script");
    let mut perms = fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script, perms).unwrap();
    script
}

#[test]
fn heartbeat_disable_does_not_pass_sock_env_to_child() {
    let dir = tempdir().expect("tempdir");
    let script = env_check_script(dir.path());
    let bin = supervisor_bin();

    let output = Command::new(&bin)
        .env("BUFFR_CHILD_BIN", &script)
        .env("RUST_LOG", "info")
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
fn crasher_script(dir: &std::path::Path) -> std::path::PathBuf {
    let script = dir.join("fake-buffr-crash");
    fs::write(&script, "#!/bin/sh\nexit 1\n").expect("write script");
    let mut perms = fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script, perms).unwrap();
    script
}

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
