//! Windows integration tests for the supervisor's restart policy.
//!
//! The Windows path now mirrors the Unix one:
//!   - exit 0 → supervisor exits 0;
//!   - a plain non-zero exit is a CLI/config error → propagate, no restart;
//!   - a hang (here: never connecting to the heartbeat pipe) → restart, and
//!     three in the 30 s window halts the supervisor non-zero.
#![cfg(windows)]

use std::process::Command;

mod common;
use common::supervisor_bin;

/// Absolute path to `cmd.exe`. `BUFFR_CHILD_BIN` must name a real file, so
/// the bare string `"cmd"` is no longer accepted.
fn cmd_exe() -> String {
    std::env::var("ComSpec").unwrap_or_else(|_| r"C:\Windows\System32\cmd.exe".to_string())
}

/// A child that never connects to the heartbeat pipe is treated as a hang.
/// Three of those inside the window halt the supervisor.
#[test]
fn three_hangs_in_window_halt_supervisor_nonzero() {
    let bin = supervisor_bin();
    // `ping -n 30 127.0.0.1` just sits there for ~29 s without ever touching
    // the heartbeat pipe. With a 1 s connect grace each cycle is ~1.25 s.
    let output = Command::new(&bin)
        .env("BUFFR_CHILD_BIN", cmd_exe())
        .env("BUFFR_CONNECT_GRACE_MS", "1000")
        .env("RUST_LOG", "info")
        .arg("--heartbeat-timeout")
        .arg("2")
        .arg("/c")
        .arg("ping")
        .arg("-n")
        .arg("30")
        .arg("127.0.0.1")
        .output()
        .expect("failed to run buffr supervisor");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "supervisor should exit non-zero after 3 hangs; got: {:?}\nstderr:\n{stderr}",
        output.status
    );
    assert!(
        stderr.contains("refusing to restart") || stderr.contains("crashes/hangs"),
        "expected watchdog halt message in stderr; got:\n{stderr}"
    );
}

/// M3: a plain non-zero exit is a CLI / config error, not a crash. The
/// supervisor propagates the code instead of re-running the same failure
/// three times and reporting a misleading "3 crashes/hangs".
#[test]
fn normal_nonzero_exit_is_propagated_without_restart() {
    let bin = supervisor_bin();
    let output = Command::new(&bin)
        .env("BUFFR_CHILD_BIN", cmd_exe())
        .env("RUST_LOG", "info")
        .arg("--heartbeat-disable")
        .arg("/c")
        .arg("exit")
        .arg("7")
        .output()
        .expect("failed to run buffr supervisor");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(7),
        "supervisor must propagate the child's exit code; stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("refusing to restart"),
        "a CLI-style failure must not be reported as 3 crashes; got:\n{stderr}"
    );
}

#[test]
fn clean_exit_causes_supervisor_exit_zero() {
    let bin = supervisor_bin();
    // `cmd /c exit 0` — clean exit.
    let status = Command::new(&bin)
        .env("BUFFR_CHILD_BIN", cmd_exe())
        .env("RUST_LOG", "info")
        .arg("--heartbeat-disable")
        .arg("/c")
        .arg("exit")
        .arg("0")
        .status()
        .expect("failed to run buffr supervisor");

    assert!(
        status.success(),
        "supervisor should exit 0 when child exits 0; got: {status:?}"
    );
}
