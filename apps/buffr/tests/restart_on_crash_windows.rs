//! Windows integration test: child crashes 3 times in window → supervisor halts non-zero.
//!
//! Uses `cmd /c exit 1` as the crasher child (exits non-zero immediately).
//! The supervisor's 3-strikes-in-30s backoff must kick in and exit non-zero.
#![cfg(windows)]

use std::process::Command;

fn supervisor_bin() -> std::path::PathBuf {
    let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // apps/buffr → apps
    p.pop(); // apps → workspace root
    p.push("target");
    p.push("debug");
    p.push("buffr.exe");
    p
}

#[test]
fn three_crashes_in_window_halts_supervisor_nonzero() {
    let bin = supervisor_bin();
    // `cmd /c exit 1` — exits immediately with code 1 (crash simulation).
    // The supervisor will restart it 3 times within the 30 s window and halt.
    let output = Command::new(&bin)
        .env("BUFFR_CHILD_BIN", "cmd")
        .env("RUST_LOG", "info")
        // Disable heartbeat so we only test exit-code restart logic.
        .arg("--heartbeat-disable")
        // Forward args: `cmd /c exit 1`
        .arg("/c")
        .arg("exit")
        .arg("1")
        .output()
        .expect("failed to run buffr supervisor");

    assert!(
        !output.status.success(),
        "supervisor should exit non-zero after 3 crashes; got: {:?}",
        output.status
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("refusing to restart") || stderr.contains("crashes in"),
        "expected watchdog halt message in stderr; got:\n{stderr}"
    );
}

#[test]
fn clean_exit_causes_supervisor_exit_zero() {
    let bin = supervisor_bin();
    // `cmd /c exit 0` — clean exit.
    let status = Command::new(&bin)
        .env("BUFFR_CHILD_BIN", "cmd")
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
