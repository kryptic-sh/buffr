//! buffr-supervisor — crash-restart watchdog for the buffr browser binary.
//!
//! **Linux only** in Round 1. On other platforms the supervisor prints a
//! notice and execs the child binary directly without any watchdog loop.
//! This keeps `cargo build --workspace` green on every platform's CI.
//!
//! ## Usage
//!
//! ```sh
//! buffr-supervisor [buffr-args...]
//! # or, for testing/dev:
//! BUFFR_CHILD_BIN=/path/to/buffr buffr-supervisor [args...]
//! ```
//!
//! ## Child resolution order
//!
//! 1. `BUFFR_CHILD_BIN` env var (test/dev override).
//! 2. `buffr` in the same directory as the supervisor's own exe.
//! 3. `buffr` on `$PATH`.

use std::ffi::OsString;
use std::path::PathBuf;

use clap::Parser;

/// Crash-restart watchdog for the buffr browser binary.
///
/// Forwards all arguments to the buffr child process and automatically
/// restarts it on crash. Stops after 3 crashes in 30 seconds and
/// points at the crash log directory.
#[derive(Debug, Parser)]
#[command(
    name = "buffr-supervisor",
    version = env!("CARGO_PKG_VERSION"),
    about = "Crash-restart watchdog for buffr. Forwards args to the buffr browser \
             binary and restarts on crash. Linux only in this release.",
    // Allow unknown args so everything after the supervisor flags is forwarded.
    allow_hyphen_values = true,
)]
struct Cli {
    /// Arguments forwarded verbatim to the buffr child process.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    child_args: Vec<OsString>,
}

fn resolve_child_bin() -> anyhow::Result<PathBuf> {
    // 1. Env override — for testing / dev.
    if let Ok(val) = std::env::var("BUFFR_CHILD_BIN") {
        let p = PathBuf::from(val);
        if p.is_file() {
            return Ok(p);
        }
        // Still return it — let exec fail with a clear OS error.
        return Ok(p);
    }

    // 2. Sibling in the same dir as our own exe.
    if let Ok(exe) = std::env::current_exe()
        && let Some(parent) = exe.parent()
    {
        let candidate = parent.join("buffr");
        if candidate.is_file() {
            return Ok(candidate);
        }
    }

    // 3. PATH lookup.
    which_buffr()
}

fn which_buffr() -> anyhow::Result<PathBuf> {
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join("buffr");
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    anyhow::bail!("buffr binary not found. Set BUFFR_CHILD_BIN or ensure buffr is on PATH.");
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let child_bin = resolve_child_bin()?;
    let child_args = cli.child_args;

    tracing::info!(
        child = %child_bin.display(),
        "buffr-supervisor starting"
    );

    #[cfg(target_os = "linux")]
    {
        linux::run_supervisor(child_bin, child_args)?;
    }

    #[cfg(not(target_os = "linux"))]
    {
        // Runtime fallback: no supervision — just exec the child directly.
        // This keeps `cargo build --workspace` green on macOS/Windows CI.
        eprintln!(
            "buffr-supervisor: watchdog not yet supported on this platform — \
             running buffr directly without supervision."
        );
        non_linux::exec_child(child_bin, child_args)?;
    }

    Ok(())
}

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use nix::sys::signal::{self, Signal};
    use nix::unistd::Pid;

    /// Rolling window for crash backoff detection.
    const WINDOW_SECS: u64 = 30;
    const CRASH_LIMIT: usize = 3;
    /// Restart cooldown between attempts.
    const RESTART_COOLDOWN: Duration = Duration::from_millis(250);
    /// How long to wait for graceful termination before SIGKILL.
    const GRACEFUL_TIMEOUT: Duration = Duration::from_secs(5);

    pub fn run_supervisor(child_bin: PathBuf, child_args: Vec<OsString>) -> anyhow::Result<()> {
        // Timestamps of the last CRASH_LIMIT crashes (rolling window).
        let mut crash_times: Vec<Instant> = Vec::new();
        let mut restart_count: u32 = 0;

        // Flag set when supervisor receives SIGINT/SIGTERM so we know
        // a forwarded-signal exit from the child is intentional.
        let shutdown_requested = Arc::new(AtomicBool::new(false));

        loop {
            // --- spawn child ---
            let spawn_time = Instant::now();
            let mut cmd = build_command(&child_bin, &child_args);
            let mut child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => {
                    anyhow::bail!("failed to spawn child {}: {e}", child_bin.display());
                }
            };

            let child_pid = Pid::from_raw(child.id() as i32);
            tracing::info!(pid = %child_pid, "child spawned");

            // Install signal forwarding for this child.
            let sr = Arc::clone(&shutdown_requested);
            let _guard = install_signal_forwarding(child_pid, sr);

            // --- wait for child ---
            let status = child.wait()?;
            let elapsed = spawn_time.elapsed();

            // Was this a clean exit?
            let exit_code = status.code();
            let is_clean = exit_code == Some(0);

            if is_clean {
                tracing::info!(
                    pid = %child_pid,
                    elapsed_ms = elapsed.as_millis(),
                    "child exited cleanly (exit 0); supervisor done"
                );
                return Ok(());
            }

            // If the supervisor itself was asked to shut down, a non-zero
            // exit from the child is the result of our own SIGTERM forward
            // — don't restart.
            if shutdown_requested.load(Ordering::SeqCst) {
                tracing::info!(
                    pid = %child_pid,
                    status = ?status,
                    "child exited after supervisor shutdown signal; not restarting"
                );
                return Ok(());
            }

            // Crash path.
            restart_count += 1;
            let now = Instant::now();
            crash_times.push(now);

            // Evict entries older than WINDOW_SECS.
            let window_start = now - Duration::from_secs(WINDOW_SECS);
            crash_times.retain(|t| *t >= window_start);

            tracing::info!(
                pid = %child_pid,
                exit_status = ?status,
                restart_count = restart_count,
                crashes_in_window = crash_times.len(),
                elapsed_ms = elapsed.as_millis(),
                "child crashed; considering restart"
            );

            if crash_times.len() >= CRASH_LIMIT {
                tracing::error!(
                    "watchdog: {CRASH_LIMIT} crashes in {WINDOW_SECS}s, refusing to restart. \
                     Check crash logs at ~/.local/share/buffr/crashes/"
                );
                std::process::exit(1);
            }

            tracing::info!(
                cooldown_ms = RESTART_COOLDOWN.as_millis(),
                "waiting before restart"
            );
            std::thread::sleep(RESTART_COOLDOWN);
        }
    }

    fn build_command(bin: &PathBuf, args: &[OsString]) -> std::process::Command {
        use std::os::unix::process::CommandExt;

        let mut cmd = std::process::Command::new(bin);
        cmd.args(args);

        // setsid: child becomes session leader + new process group.
        // This isolates the child's pgrp from the supervisor's so we can
        // cleanly killpg the entire CEF helper tree without hitting ourselves.
        unsafe {
            cmd.pre_exec(|| {
                nix::unistd::setsid()
                    .map(|_| ())
                    .map_err(|e| std::io::Error::from_raw_os_error(e as i32))
            });
        }

        cmd
    }

    /// Install handlers for SIGINT and SIGTERM.
    ///
    /// On receipt, forward the signal to the child's process group via
    /// `killpg`, wait up to GRACEFUL_TIMEOUT for the child to exit, then
    /// SIGKILL if still alive. Sets `shutdown_requested` so the main loop
    /// knows not to restart after the forwarded signal.
    ///
    /// Returns a join handle; dropping it leaves the thread running but
    /// that's fine — the process is exiting anyway at that point.
    fn install_signal_forwarding(
        child_pid: Pid,
        shutdown_requested: Arc<AtomicBool>,
    ) -> std::thread::JoinHandle<()> {
        use signal_hook::consts::signal::{SIGINT, SIGTERM};
        use signal_hook::iterator::Signals;

        std::thread::spawn(move || {
            let mut signals = match Signals::new([SIGINT, SIGTERM]) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("failed to install signal handlers: {e}");
                    return;
                }
            };

            // Block until the first signal arrives.
            if let Some(sig) = signals.forever().next() {
                tracing::info!(
                    signal = sig,
                    "supervisor received signal; forwarding to child pgrp"
                );
                shutdown_requested.store(true, Ordering::SeqCst);

                // child_pid IS the pgid since the child called setsid().
                if let Err(e) = signal::killpg(child_pid, Signal::SIGTERM) {
                    tracing::warn!(error = %e, "killpg SIGTERM failed");
                }

                // Wait up to GRACEFUL_TIMEOUT then SIGKILL.
                let deadline = Instant::now() + GRACEFUL_TIMEOUT;
                loop {
                    std::thread::sleep(Duration::from_millis(100));
                    // Check if process is gone via kill(pid, 0).
                    if matches!(signal::kill(child_pid, None), Err(nix::errno::Errno::ESRCH)) {
                        break;
                    }
                    if Instant::now() >= deadline {
                        tracing::warn!(pid = %child_pid, "graceful timeout; sending SIGKILL to pgrp");
                        let _ = signal::killpg(child_pid, Signal::SIGKILL);
                        break;
                    }
                }
            }
        })
    }
}

#[cfg(not(target_os = "linux"))]
mod non_linux {
    use std::ffi::OsString;
    use std::path::PathBuf;

    /// Exec the child directly without any supervision.
    pub fn exec_child(child_bin: PathBuf, args: Vec<OsString>) -> anyhow::Result<()> {
        use std::process::Command;

        let status = Command::new(&child_bin)
            .args(&args)
            .status()
            .map_err(|e| anyhow::anyhow!("failed to exec {}: {e}", child_bin.display()))?;

        std::process::exit(status.code().unwrap_or(1));
    }
}
