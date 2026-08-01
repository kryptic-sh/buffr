//! Integration test: fake child connects + pings every 500 ms for 12 s.
//! Supervisor must NOT kill it (i.e. it must exit 0 at the end).
#![cfg(unix)]

use std::process::Command;
use tempfile::tempdir;

mod common;
use common::{supervisor_bin, write_script};

/// Write a script that:
///   1. Reads BUFFR_SUPERVISOR_SOCK and connects.
///   2. Pings every 500 ms for 12 s.
///   3. Exits 0.
///
/// This proves the supervisor does NOT fire the hang detector while
/// the child is alive and pinging.
fn pinger_script(dir: &std::path::Path) -> std::path::PathBuf {
    // We use a tiny Python3 script to avoid a socat dependency.
    let content = r#"#!/usr/bin/env python3
import os, socket, time, sys

sock_path = os.environ.get("BUFFR_SUPERVISOR_SOCK")
if not sock_path:
    # No supervisor — just sleep and exit cleanly.
    time.sleep(2)
    sys.exit(0)

s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(sock_path)

end = time.monotonic() + 12
while time.monotonic() < end:
    s.send(b"\x01")
    time.sleep(0.5)

s.close()
sys.exit(0)
"#;
    write_script(dir, "fake-buffr-alive", content)
}

#[test]
fn supervisor_does_not_kill_pinging_child() {
    let dir = tempdir().expect("tempdir");
    let script = pinger_script(dir.path());
    let bin = supervisor_bin();

    // Run with default timeout (8 s); child pings for 12 s then exits 0.
    // Supervisor should exit 0 (clean child exit).
    let status = Command::new(&bin)
        .env("BUFFR_CHILD_BIN", &script)
        .env("RUST_LOG", "debug")
        .arg("--heartbeat-timeout")
        .arg("8")
        .status()
        .expect("failed to run buffr");

    assert!(
        status.success(),
        "supervisor should exit 0 when child pings regularly; got: {status:?}"
    );
}
