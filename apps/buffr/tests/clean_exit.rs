//! Integration test: child exits 0 → supervisor exits 0, no restart.
#![cfg(unix)]

use std::process::Command;

mod common;
use common::supervisor_bin;

#[test]
fn child_exit_zero_causes_supervisor_exit_zero() {
    let bin = supervisor_bin();
    // Use /bin/true as child: exits 0 immediately.
    let status = Command::new(&bin)
        .env("BUFFR_CHILD_BIN", "/bin/true")
        .env("RUST_LOG", "info")
        .status()
        .expect("failed to run buffr");

    assert!(
        status.success(),
        "supervisor should exit 0 when child exits 0; got: {status:?}"
    );
}

/// `BUFFR_CHILD_BIN` pointing at something that is not a file must fail
/// loudly rather than producing an opaque spawn error later.
#[test]
fn bogus_child_bin_override_fails_with_a_clear_error() {
    let bin = supervisor_bin();
    let output = Command::new(&bin)
        .env("BUFFR_CHILD_BIN", "/definitely/not/a/real/binary")
        .env("RUST_LOG", "info")
        .output()
        .expect("failed to run buffr");

    assert!(
        !output.status.success(),
        "supervisor should fail when BUFFR_CHILD_BIN is not a file"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("BUFFR_CHILD_BIN"),
        "expected a BUFFR_CHILD_BIN diagnostic; got:\n{stderr}"
    );
}
