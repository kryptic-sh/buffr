//! Windows integration test: child exits 0 → supervisor exits 0, no restart.
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
fn child_exit_zero_causes_supervisor_exit_zero() {
    let bin = supervisor_bin();
    // `cmd /c exit 0` exits cleanly — supervisor must not restart.
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
