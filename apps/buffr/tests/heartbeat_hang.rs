//! Integration tests for hang detection and backoff.
//!
//! - `three_hangs_trigger_backoff_halt`: the child connects, sends one ping,
//!   then sleeps 30 s while holding the socket open. With
//!   `--heartbeat-timeout 2` the supervisor kills at ~2 s and restarts; after
//!   three hangs in the window it halts non-zero.
//!
//! - `disconnect_while_alive_triggers_restart` (H12): the child connects,
//!   pings once, **closes the socket**, and then keeps running. The
//!   supervisor used to read EOF, conclude the child had exited, and block
//!   forever in `wait()` on a frozen browser. It must instead give the child
//!   a short grace to actually exit, then kill and restart it.
#![cfg(unix)]

use std::process::Command;
use std::time::{Duration, Instant};
use tempfile::tempdir;

mod common;
use common::{supervisor_bin, write_script};

/// Child that connects, pings once, then hangs for 30 s holding the socket.
fn one_ping_then_hang_script(dir: &std::path::Path) -> std::path::PathBuf {
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
    write_script(dir, "fake-buffr-hang", content)
}

/// Child that connects, pings once, CLOSES the socket, then keeps running.
///
/// This is what a wedged UI looked like before the heartbeat thread was
/// changed to hold the socket open: the supervisor sees EOF while the
/// process is still very much alive.
fn ping_then_close_then_hang_script(dir: &std::path::Path) -> std::path::PathBuf {
    let content = r#"#!/usr/bin/env python3
import os, socket, time, sys

sock_path = os.environ.get("BUFFR_SUPERVISOR_SOCK")
if not sock_path:
    time.sleep(60)
    sys.exit(0)

s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(sock_path)
s.send(b"\x01")
s.close()
# Still running, but the supervisor can no longer hear us.
time.sleep(60)
sys.exit(0)
"#;
    write_script(dir, "fake-buffr-disconnect", content)
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
        .expect("failed to run buffr");

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

/// H12 regression: a child that drops the heartbeat socket but stays alive
/// must be killed and restarted.
///
/// Before the fix the supervisor mapped EOF to `Disconnected` → "not a hang"
/// → `try_wait()` returns `None` → `wait()` blocks forever, so this test
/// would hang until the harness timeout instead of halting.
#[test]
fn disconnect_while_alive_triggers_restart() {
    let dir = tempdir().expect("tempdir");
    let script = ping_then_close_then_hang_script(dir.path());
    let bin = supervisor_bin();

    let start = Instant::now();
    let output = Command::new(&bin)
        .env("BUFFR_CHILD_BIN", &script)
        .env("RUST_LOG", "info")
        .arg("--heartbeat-timeout")
        .arg("2")
        .output()
        .expect("failed to run buffr");
    let elapsed = start.elapsed();

    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "supervisor should exit non-zero after 3 disconnect-while-alive hangs; \
         got: {:?}\nstderr:\n{stderr}",
        output.status
    );
    assert!(
        stderr.contains("refusing to restart"),
        "expected the backoff halt message (i.e. it actually restarted); got:\n{stderr}"
    );
    assert!(
        stderr.contains("still running"),
        "expected the disconnect-while-alive diagnostic; got:\n{stderr}"
    );
    // Three cycles of (≈2 s grace + 250 ms cooldown). Comfortably below the
    // child's own 60 s sleep, which is what a blocked `wait()` would cost.
    assert!(
        elapsed < Duration::from_secs(30),
        "supervisor took {elapsed:?} — it likely blocked in wait() on the live child"
    );
}
