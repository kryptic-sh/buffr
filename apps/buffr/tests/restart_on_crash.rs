//! Integration test: child crashes 3 times in window → supervisor halts non-zero.
#![cfg(unix)]

use std::process::Command;
use tempfile::tempdir;

mod common;
use common::{crasher_script, supervisor_bin, write_script};

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
    // A child that dies via SIGABRT the first time, then exits 0.
    // We achieve this via a counter file: first run → abort, second → exit 0.
    let dir = tempdir().expect("tempdir");
    let counter_file = dir.path().join("count");

    // Same rationale as `crasher_script`: `exit 1` is a normal exit and
    // would be propagated rather than restarted.
    let script_content = format!(
        "#!/bin/sh\n\
         COUNT_FILE=\"{}\"\n\
         COUNT=0\n\
         if [ -f \"$COUNT_FILE\" ]; then COUNT=$(cat \"$COUNT_FILE\"); fi\n\
         COUNT=$((COUNT+1))\n\
         echo $COUNT > \"$COUNT_FILE\"\n\
         if [ \"$COUNT\" -ge 2 ]; then exit 0; fi\n\
         kill -ABRT $$\n",
        counter_file.display()
    );

    let script = write_script(dir.path(), "staged-buffr", &script_content);

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

/// A plain non-zero exit is a CLI / config error, not a crash: the
/// supervisor propagates the code and does NOT burn through the
/// three-strikes window re-running the same failure.
#[test]
fn normal_nonzero_exit_is_propagated_without_restart() {
    let dir = tempdir().expect("tempdir");
    let script = write_script(dir.path(), "exit-7", "#!/bin/sh\nexit 7\n");
    let bin = supervisor_bin();

    let output = Command::new(&bin)
        .env("BUFFR_CHILD_BIN", &script)
        .env("RUST_LOG", "info")
        .output()
        .expect("failed to run buffr");

    assert_eq!(
        output.status.code(),
        Some(7),
        "supervisor must propagate the child's exit code"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("refusing to restart"),
        "a CLI-style failure must not be reported as 3 crashes; got:\n{stderr}"
    );
}

/// M4: the clean-shutdown flag must work even with `--heartbeat-disable`,
/// where there is no heartbeat socket to derive a path from. Without it, a
/// segfault during CEF/wgpu teardown after the user closed the window is
/// treated as a crash and relaunched three times.
#[test]
fn clean_flag_suppresses_restart_with_heartbeat_disabled() {
    let dir = tempdir().expect("tempdir");
    // Touch the flag the supervisor handed us, then die via SIGABRT.
    let script = write_script(
        dir.path(),
        "clean-then-abort",
        "#!/bin/sh\n\
         if [ -z \"$BUFFR_SUPERVISOR_CLEAN_FLAG\" ]; then\n\
             echo \"FAIL: BUFFR_SUPERVISOR_CLEAN_FLAG not set\" >&2\n\
             exit 3\n\
         fi\n\
         : > \"$BUFFR_SUPERVISOR_CLEAN_FLAG\"\n\
         kill -ABRT $$\n",
    );
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
        "clean flag must suppress the restart even without a heartbeat socket; \
         got: {:?}\nstderr:\n{stderr}",
        output.status
    );
    assert!(
        !stderr.contains("refusing to restart"),
        "expected no crash loop; got:\n{stderr}"
    );
}
