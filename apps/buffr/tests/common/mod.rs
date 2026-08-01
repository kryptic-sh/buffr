//! Shared helpers for the `buffr` supervisor integration tests.
//!
//! Not every test uses every helper, hence the crate-level `dead_code`
//! allowance: this module is `include`d into each integration-test crate
//! rather than compiled once.

#![allow(dead_code, unused_imports)]

use std::path::PathBuf;

/// Absolute path to the supervisor binary built for *this* test run.
///
/// Cargo sets `CARGO_BIN_EXE_<name>` for integration tests, so this is
/// correct under `CARGO_TARGET_DIR`, `--release` and `--target <triple>`
/// alike. Hand-rolling `<workspace>/target/debug/buffr` either panics or —
/// worse — silently tests a stale binary from an earlier build.
pub fn supervisor_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_buffr"))
}

#[cfg(unix)]
mod unix_helpers {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    /// Write `content` to `dir/name` and mark it executable.
    pub fn write_script(dir: &Path, name: &str, content: &str) -> PathBuf {
        let script = dir.join(name);
        fs::write(&script, content).expect("write script");
        let mut perms = fs::metadata(&script).expect("stat script").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script, perms).expect("chmod script");
        script
    }

    /// A child that SIGABRTs itself.
    ///
    /// The supervisor only treats abnormal deaths (`ExitStatus::code() ==
    /// None`, i.e. killed by a signal) as restart-eligible crashes — a plain
    /// `exit 1` is a normal exit and gets propagated without restart.
    pub fn crasher_script(dir: &Path) -> PathBuf {
        write_script(dir, "fake-buffr-crash", "#!/bin/sh\nkill -ABRT $$\n")
    }
}

#[cfg(unix)]
pub use unix_helpers::{crasher_script, write_script};
