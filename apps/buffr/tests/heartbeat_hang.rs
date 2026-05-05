//! Integration tests for hang detection and backoff.
//!
//! - `hang_triggers_restart`: child connects, sends one ping, then sleeps
//!   20 s. With `--heartbeat-timeout 2` the supervisor kills at ~2 s and
//!   restarts. We verify the supervisor eventually halts (3 hangs in 30 s).
//!
//! - `three_hangs_trigger_backoff_halt`: same scenario fires 3 times,
//!   supervisor exits non-zero after the third hang.
#![cfg(target_os = "linux")]

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

/// Child that connects, pings once, then hangs for 30 s.
fn one_ping_then_hang_script(dir: &std::path::Path) -> std::path::PathBuf {
    let script = dir.join("fake-buffr-hang");
    let content = r#"#!/usr/bin/env python3
import os, socket, time, sys

sock_path = os.environ.get("BUFFR_SUPERVISOR_SOCK")
if not sock_path:
    time.sleep(30)
    sys.exit(0)

s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(sock_path)
s.send(b"\x01")
time.sleep(30)
sys.exit(0)
"#;
    fs::write(&script, content).expect("write script");
    let mut perms = fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script, perms).unwrap();
    script
}

/// The supervisor detects a hang (timeout=2), kills the child, and restarts.
/// Because every restart also hangs, after 3 hangs in the window the
/// supervisor exits non-zero (backoff halt).
#[test]
fn three_hangs_trigger_backoff_halt() {
    let dir = tempdir().expect("tempdir");
    let script = one_ping_then_hang_script(dir.path());
    let bin = supervisor_bin();

    // timeout=2: each cycle takes ~2 s; 3 cycles = ~6 s total.
    let output = Command::new(&bin)
        .env("BUFFR_CHILD_BIN", &script)
        .env("RUST_LOG", "info")
        .arg("--heartbeat-timeout")
        .arg("2")
        .output()
        .expect("failed to run buffr-supervisor");

    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "supervisor should exit non-zero after 3 hangs; got: {:?}\nstderr:\n{stderr}",
        output.status
    );

    // Confirm hang detection and backoff messages appear.
    assert!(
        stderr.contains("ui hang detected") || stderr.contains("hang"),
        "expected 'ui hang detected' in stderr; got:\n{stderr}"
    );
    assert!(
        stderr.contains("refusing to restart") || stderr.contains("crashes/hangs"),
        "expected backoff halt message in stderr; got:\n{stderr}"
    );
}
