//! Integration test: child exits 0 → supervisor exits 0, no restart.
#![cfg(target_os = "linux")]

use std::process::Command;

fn supervisor_bin() -> std::path::PathBuf {
    // Built by `cargo test --test clean_exit -p buffr-supervisor`.
    let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // Walk up to workspace root, then into target.
    p.pop(); // apps/buffr-supervisor → apps
    p.pop(); // apps → workspace root
    p.push("target");
    p.push("debug");
    p.push("buffr-supervisor");
    p
}

#[test]
fn child_exit_zero_causes_supervisor_exit_zero() {
    let bin = supervisor_bin();
    // Use /bin/true as child: exits 0 immediately.
    let status = Command::new(&bin)
        .env("BUFFR_CHILD_BIN", "/bin/true")
        .env("RUST_LOG", "info")
        .status()
        .expect("failed to run buffr-supervisor");

    assert!(
        status.success(),
        "supervisor should exit 0 when child exits 0; got: {status:?}"
    );
}
