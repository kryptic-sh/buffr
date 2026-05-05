//! Integration test: child crashes 3 times in window → supervisor halts non-zero.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use tempfile::tempdir;

fn supervisor_bin() -> std::path::PathBuf {
    let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // apps/buffr → apps
    p.pop(); // apps → workspace root
    p.push("target");
    p.push("debug");
    p.push("buffr");
    p
}

/// Write a tiny shell script that always exits non-zero.
fn crasher_script(dir: &std::path::Path) -> std::path::PathBuf {
    let script = dir.join("fake-buffr");
    fs::write(&script, "#!/bin/sh\nexit 1\n").expect("write script");
    let mut perms = fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script, perms).unwrap();
    script
}

#[test]
fn three_crashes_in_window_halts_supervisor_nonzero() {
    let dir = tempdir().expect("tempdir");
    let crasher = crasher_script(dir.path());
    let bin = supervisor_bin();

    let output = Command::new(&bin)
        .env("BUFFR_CHILD_BIN", &crasher)
        .env("RUST_LOG", "info")
        .output()
        .expect("failed to run buffr");

    assert!(
        !output.status.success(),
        "supervisor should exit non-zero after 3 crashes; got: {:?}",
        output.status
    );

    // Confirm the watchdog halt message appears in stderr.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("refusing to restart") || stderr.contains("crashes in"),
        "expected watchdog halt message in stderr; got:\n{stderr}"
    );
}

#[test]
fn single_crash_then_success_no_halt() {
    // A child that exits 1 the first time, then 0.
    // We achieve this via a counter file: first run → exit 1, second → exit 0.
    let dir = tempdir().expect("tempdir");
    let counter_file = dir.path().join("count");

    // Write script that reads a counter, increments, exits 1 until count >= 2.
    let script_content = format!(
        "#!/bin/sh\n\
         COUNT_FILE=\"{}\"\n\
         COUNT=0\n\
         if [ -f \"$COUNT_FILE\" ]; then COUNT=$(cat \"$COUNT_FILE\"); fi\n\
         COUNT=$((COUNT+1))\n\
         echo $COUNT > \"$COUNT_FILE\"\n\
         if [ \"$COUNT\" -ge 2 ]; then exit 0; fi\n\
         exit 1\n",
        counter_file.display()
    );

    let script = dir.path().join("staged-buffr");
    fs::write(&script, &script_content).expect("write script");
    let mut perms = fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script, perms).unwrap();

    let bin = supervisor_bin();
    let status = Command::new(&bin)
        .env("BUFFR_CHILD_BIN", &script)
        .env("RUST_LOG", "info")
        .status()
        .expect("failed to run buffr");

    assert!(
        status.success(),
        "supervisor should exit 0 after child eventually succeeds; got: {status:?}"
    );
}
