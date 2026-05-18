//! Platform-specific "open this file with the user's launcher" helper.
//!
//! Used by [`crate::handlers::BuffrDownloadHandler`] when a download
//! finishes and `DownloadsConfig::open_on_finish` is `true`. Kept in
//! its own module so the spawn path is testable in isolation via the
//! [`Spawn`] trait — the CEF handler is hard to unit-test (it needs a
//! live `DownloadItem`), but the dispatch decision is plain Rust.
//!
//! ## Platform commands
//!
//! - **Linux**: `xdg-open <path>`
//! - **macOS**: `open <path>`
//! - **Windows**: `cmd /c start "" <path>`
//!
//! Failures are logged at `warn!` and otherwise swallowed — the
//! browser thread never blocks waiting for the user's launcher to
//! respond.

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::Command;

use tracing::warn;

/// Spawnable command abstraction. Production uses [`OsSpawn`] which
/// shells out via `std::process::Command`; tests substitute
/// [`RecordSpawn`] to capture argv without actually launching anything.
pub trait Spawn: Send + Sync + 'static {
    /// Spawn `program` with `args`. Implementations must NOT block —
    /// they should fire-and-forget the child. Return value is `Ok(())`
    /// on successful spawn, `Err(reason)` on failure.
    fn spawn(&self, program: &OsStr, args: &[&OsStr]) -> Result<(), String>;
}

/// Real-process spawner used in production. `Command::spawn` is
/// non-blocking: it returns once the child is forked, not when it
/// exits.
#[derive(Debug, Default, Clone, Copy)]
pub struct OsSpawn;

impl Spawn for OsSpawn {
    fn spawn(&self, program: &OsStr, args: &[&OsStr]) -> Result<(), String> {
        match Command::new(program).args(args).spawn() {
            Ok(_) => Ok(()),
            Err(e) => Err(format!("{e}")),
        }
    }
}

/// Open `path` via the platform's launcher using `spawn`. Logs a
/// `warn!` on spawn failure but never propagates the error — the
/// browser thread isn't allowed to fail because the user's launcher
/// misbehaves.
pub fn open_path<S: Spawn>(spawn: &S, path: &Path) {
    let path_os = path.as_os_str();
    let (program, args) = command_for(path_os);
    let arg_refs: Vec<&OsStr> = args.iter().map(OsString::as_os_str).collect();
    if let Err(reason) = spawn.spawn(&program, &arg_refs) {
        warn!(
            path = %path.display(),
            reason,
            "open_on_finish: spawn failed"
        );
    }
}

/// Resolve `(program, argv)` for the current platform. Pulled out of
/// [`open_path`] so tests can assert the exact command shape on each
/// platform without spinning a real subprocess.
pub fn command_for(path: &OsStr) -> (OsString, Vec<OsString>) {
    #[cfg(target_os = "linux")]
    {
        (OsString::from("xdg-open"), vec![path.to_os_string()])
    }
    #[cfg(target_os = "macos")]
    {
        (OsString::from("open"), vec![path.to_os_string()])
    }
    #[cfg(target_os = "windows")]
    {
        // `start ""` consumes the empty title argument so the actual
        // path-arg isn't misread as a window title. `cmd /c` runs
        // `start` and exits; nothing to forward stdout to.
        (
            OsString::from("cmd"),
            vec![
                OsString::from("/c"),
                OsString::from("start"),
                OsString::from(""),
                path.to_os_string(),
            ],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::sync::Mutex;

    /// Records every spawn invocation so tests can assert argv shape.
    /// Always succeeds.
    #[derive(Default)]
    struct RecordSpawn {
        calls: Mutex<Vec<(OsString, Vec<OsString>)>>,
    }

    impl Spawn for RecordSpawn {
        fn spawn(&self, program: &OsStr, args: &[&OsStr]) -> Result<(), String> {
            let mut calls = self.calls.lock().expect("lock");
            calls.push((
                program.to_os_string(),
                args.iter().map(|a| a.to_os_string()).collect(),
            ));
            Ok(())
        }
    }

    /// Spawner that always errors. Lets us hit the warn-and-swallow
    /// branch without a real failing process.
    struct FailSpawn;

    impl Spawn for FailSpawn {
        fn spawn(&self, _program: &OsStr, _args: &[&OsStr]) -> Result<(), String> {
            Err("boom".into())
        }
    }

    #[test]
    fn open_path_invokes_spawn() {
        let s = RecordSpawn::default();
        open_path(&s, Path::new("/tmp/foo"));
        let calls = s.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        let (prog, args) = &calls[0];
        #[cfg(target_os = "linux")]
        {
            assert_eq!(prog, &OsString::from("xdg-open"));
            assert_eq!(args, &vec![OsString::from("/tmp/foo")]);
        }
        #[cfg(target_os = "macos")]
        {
            assert_eq!(prog, &OsString::from("open"));
            assert_eq!(args, &vec![OsString::from("/tmp/foo")]);
        }
        #[cfg(target_os = "windows")]
        {
            assert_eq!(prog, &OsString::from("cmd"));
            assert_eq!(args.len(), 4);
            assert_eq!(args[0], OsString::from("/c"));
            assert_eq!(args[1], OsString::from("start"));
        }
    }

    #[test]
    fn open_path_swallows_spawn_failure() {
        // Should not panic. The warn log is a side-effect we don't
        // assert on here — the contract is "never propagate".
        open_path(&FailSpawn, Path::new("/tmp/foo"));
    }

    #[test]
    fn command_for_returns_path_in_argv() {
        let (_prog, args) = command_for(OsStr::new("/tmp/foo"));
        assert!(args.iter().any(|a| a == &OsString::from("/tmp/foo")));
    }
}
