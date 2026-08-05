//! buffr — crash-restart + hang watchdog supervisor for the buffr browser.
//!
//! **Linux and macOS** use Unix-domain-socket heartbeats + `setsid`/`killpg`.
//! **Windows** (Round 5) uses Job Objects with `KILL_ON_JOB_CLOSE` and a
//! named-pipe heartbeat.  On all other platforms the supervisor prints a
//! notice and execs the child binary directly without a watchdog loop.
//! This keeps `cargo build --workspace` green on every platform's CI.
//!
//! The platform-independent parts of the loop — waiting for the child to
//! connect, watching the heartbeat, the rolling crash window and the
//! restart/propagate/stop decision — live in [`supervisor`] and are generic
//! over a [`supervisor::ChildHandle`].  Each platform only supplies its own
//! "is the child alive / how did it die / kill it" primitive.
//!
//! ## Runtime files
//!
//! Both the heartbeat socket and the clean-shutdown flag live inside a
//! private per-uid directory (`$XDG_RUNTIME_DIR/buffr`, else
//! `$TMPDIR/buffr-<uid>` created `0700` and verified after creation).  A
//! world-writable `/tmp/buffr-<pid>.*` is derivable by any local user, who
//! could pre-create the clean flag to disable the crash watchdog or squat
//! the socket path to disable hang detection.
//!
//! On macOS `XDG_RUNTIME_DIR` is normally unset, so the socket lands in
//! `$TMPDIR/buffr-<uid>/buffr-<pid>.sock`.  `$TMPDIR` there is roughly
//! `/var/folders/xx/<hash>/T/` (~50 bytes), leaving the full path well
//! inside Darwin's ~104-byte `sun_path` limit.
//!
//! ## Usage
//!
//! ```sh
//! buffr [buffr-app-args...]
//! # or, for testing/dev:
//! BUFFR_CHILD_BIN=/path/to/buffr-app buffr [args...]
//! ```
//!
//! ## Child resolution order
//!
//! 1. `BUFFR_CHILD_BIN` env var (test/dev override).
//! 2. `buffr-app` in the same directory as the supervisor's own exe.
//! 3. `buffr-app` on `$PATH`.

use std::ffi::OsString;
use std::path::PathBuf;

use clap::Parser;

#[cfg(any(unix, windows))]
mod supervisor;

/// Crash-restart + hang watchdog supervisor for the buffr browser.
///
/// Spawns `buffr-app` (the browser binary) and automatically restarts it
/// on crash or UI hang. Stops after 3 crashes/hangs in 30 seconds and
/// points at the crash log directory. Linux and macOS in this release.
#[derive(Debug, Parser)]
#[command(
    name = "buffr",
    version = env!("CARGO_PKG_VERSION"),
    about = "Crash-restart + hang watchdog for buffr-app. Forwards args to the buffr-app \
             browser binary and restarts on crash or hang. Linux only in this release.",
    // Allow unknown args so everything after the supervisor flags is forwarded.
    allow_hyphen_values = true,
)]
struct Cli {
    /// How many seconds of silence from the child's heartbeat before treating
    /// it as a hang and killing the process tree (default: 8).
    #[arg(long, default_value_t = 8, value_name = "SEC")]
    heartbeat_timeout: u64,

    /// Disable the UDS heartbeat entirely. The supervisor only watches exit
    /// codes (Round-1 behaviour). Useful when attaching a debugger or
    /// deliberately stopping the child with SIGSTOP for testing.
    #[arg(long)]
    heartbeat_disable: bool,

    /// Arguments forwarded verbatim to the buffr child process.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    child_args: Vec<OsString>,
}

/// `buffr-app` filename with the platform's executable suffix
/// (`buffr-app.exe` on Windows, `buffr-app` elsewhere). `is_file()`
/// is exact-name on Windows — joining the unsuffixed string misses
/// the actual `.exe` and falls through to a misleading PATH error.
fn child_bin_name() -> &'static str {
    if cfg!(windows) {
        "buffr-app.exe"
    } else {
        "buffr-app"
    }
}

fn resolve_child_bin() -> anyhow::Result<PathBuf> {
    // 1. Env override — for testing / dev.
    if let Ok(val) = std::env::var("BUFFR_CHILD_BIN") {
        let p = PathBuf::from(val);
        // The override must name a real file. Falling through with an
        // unusable path only produces an opaque `spawn` failure later.
        if !p.is_file() {
            anyhow::bail!(
                "BUFFR_CHILD_BIN is set to {} but that is not a file. \
                 Point it at the buffr-app executable (an absolute path), \
                 or unset it to use the sibling / PATH lookup.",
                p.display()
            );
        }
        return Ok(p);
    }

    let name = child_bin_name();

    // 2. Sibling in the same dir as our own exe.
    if let Ok(exe) = std::env::current_exe()
        && let Some(parent) = exe.parent()
    {
        let candidate = parent.join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }

    // 3. PATH lookup.
    which_buffr()
}

fn which_buffr() -> anyhow::Result<PathBuf> {
    let name = child_bin_name();
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    anyhow::bail!(
        "buffr-app binary not found. Set BUFFR_CHILD_BIN or ensure buffr-app is on PATH."
    );
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
    let heartbeat_timeout = std::time::Duration::from_secs(cli.heartbeat_timeout);
    let heartbeat_disable = cli.heartbeat_disable;

    tracing::info!(
        child = %child_bin.display(),
        heartbeat_timeout_s = cli.heartbeat_timeout,
        heartbeat_disable,
        "buffr supervisor starting"
    );

    #[cfg(unix)]
    {
        unix::run_supervisor(child_bin, child_args, heartbeat_timeout, heartbeat_disable)?;
    }

    #[cfg(windows)]
    {
        windows::run_supervisor(child_bin, child_args, heartbeat_timeout, heartbeat_disable)?;
    }

    #[cfg(not(any(unix, windows)))]
    {
        // Runtime fallback: no supervision — just exec the child directly.
        // This keeps `cargo build --workspace` green on exotic CI targets.
        eprintln!(
            "buffr: watchdog not supported on this platform — \
             running buffr-app directly without supervision."
        );
        other::exec_child(child_bin, child_args)?;
    }

    Ok(())
}

#[cfg(unix)]
mod unix {
    use std::ffi::OsString;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    use std::os::unix::net::UnixListener;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
    use std::time::{Duration, Instant};

    use nix::sys::signal::{self, Signal};
    use nix::unistd::Pid;

    use crate::supervisor::{
        ChildHandle, ConnectResult, CrashWindow, Disposition, ExitInfo, HeartbeatEvent,
        WatchOutcome, classify, wait_for_connect, watch_heartbeat,
    };

    /// Rolling window for crash backoff detection.
    const WINDOW_SECS: u64 = 30;
    const CRASH_LIMIT: usize = 3;
    /// Restart cooldown between attempts.
    const RESTART_COOLDOWN: Duration = Duration::from_millis(250);
    /// How long to wait for graceful termination before SIGKILL.
    const GRACEFUL_TIMEOUT: Duration = Duration::from_secs(5);
    /// How long to wait for the child to connect to the heartbeat socket.
    /// Sized to absorb cold-disk first-run setup (CEF lib load, SQLite db
    /// init, etc.). 5 s was too tight on slower disks. Integration tests
    /// override via `BUFFR_CONNECT_GRACE_MS` to keep their runtime bounded.
    const DEFAULT_CONNECT_GRACE: Duration = Duration::from_secs(20);
    /// Additional grace after the child connects before enforcing heartbeat.
    const POST_CONNECT_GRACE: Duration = Duration::from_millis(1500);
    /// Slack added to the connect grace to derive the accept thread's own
    /// deadline, so the thread always outlives the main loop's wait but
    /// still terminates instead of spinning forever (M7).
    const ACCEPT_DEADLINE_SLACK: Duration = Duration::from_secs(2);

    fn connect_grace() -> Duration {
        std::env::var("BUFFR_CONNECT_GRACE_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_CONNECT_GRACE)
    }

    /// Env var name written into the child's environment with the UDS path.
    pub const SUPERVISOR_SOCK_ENV: &str = "BUFFR_SUPERVISOR_SOCK";

    /// Env var name written into the child's environment with a path the
    /// child should touch when it wants the supervisor to treat the
    /// subsequent process exit as a clean shutdown (don't restart),
    /// regardless of the actual exit status. Used so a slow CEF /
    /// wgpu teardown that happens to segfault on the way out doesn't
    /// trigger a respawn after the user explicitly closed the window.
    pub const SUPERVISOR_CLEAN_FLAG_ENV: &str = "BUFFR_SUPERVISOR_CLEAN_FLAG";

    // ── Child handle ─────────────────────────────────────────────────────────

    /// A spawned `buffr-app` plus its process group, exposed to the shared
    /// supervisor logic through [`ChildHandle`].
    ///
    /// `exit` memoises the reaped status: `try_wait`/`wait` may only be
    /// harvested once, but the shared logic polls repeatedly.
    struct UnixChild {
        child: std::process::Child,
        /// The child called `setsid()`, so its pid IS its process-group id.
        pgid: Pid,
        exit: Option<ExitInfo>,
    }

    impl UnixChild {
        fn new(child: std::process::Child) -> Self {
            let pgid = Pid::from_raw(child.id() as i32);
            Self {
                child,
                pgid,
                exit: None,
            }
        }
    }

    /// `ExitStatus::code()` is `Some(_)` only for a normal exit; `None` means
    /// the child was killed by a signal, which is the genuine crash case.
    fn exit_info(status: std::process::ExitStatus) -> ExitInfo {
        ExitInfo {
            code: status.code(),
            crashed: status.code().is_none(),
        }
    }

    impl ChildHandle for UnixChild {
        fn pid(&self) -> u32 {
            self.child.id()
        }

        fn poll_exit(&mut self) -> Option<ExitInfo> {
            if let Some(e) = self.exit {
                return Some(e);
            }
            match self.child.try_wait() {
                Ok(Some(s)) => {
                    let e = exit_info(s);
                    self.exit = Some(e);
                    Some(e)
                }
                Ok(None) => None,
                Err(err) => {
                    tracing::warn!(error = %err, "child.try_wait() failed; treating as crash");
                    let e = ExitInfo {
                        code: None,
                        crashed: true,
                    };
                    self.exit = Some(e);
                    Some(e)
                }
            }
        }

        fn wait_exit(&mut self) -> ExitInfo {
            if let Some(e) = self.exit {
                return e;
            }
            let e = match self.child.wait() {
                Ok(s) => exit_info(s),
                Err(err) => {
                    tracing::warn!(error = %err, "child.wait() failed");
                    ExitInfo {
                        code: None,
                        crashed: true,
                    }
                }
            };
            self.exit = Some(e);
            e
        }

        fn kill_and_reap(&mut self) -> ExitInfo {
            if let Some(e) = self.exit {
                return e;
            }
            // killpg, not kill: the child is a session leader, so this takes
            // out the whole CEF helper tree in one shot.
            let _ = signal::killpg(self.pgid, Signal::SIGKILL);
            self.wait_exit()
        }
    }

    // ── Private runtime directory ────────────────────────────────────────────

    fn own_uid() -> u32 {
        nix::unistd::getuid().as_raw()
    }

    /// Create (if needed) and validate a directory that only we can enter.
    ///
    /// Rejects anything that is not a real directory owned by our uid with no
    /// group/other permission bits — that covers a symlink, a squatted path,
    /// and a shared directory another local user could write into.
    fn ensure_private_dir(path: &Path) -> anyhow::Result<()> {
        match std::fs::DirBuilder::new().mode(0o700).create(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => anyhow::bail!("creating {}: {e}", path.display()),
        }

        // symlink_metadata, not metadata: a symlink planted at this path must
        // be rejected, not silently followed to whatever it points at.
        let md = std::fs::symlink_metadata(path)
            .map_err(|e| anyhow::anyhow!("stat {}: {e}", path.display()))?;
        if !md.is_dir() {
            anyhow::bail!("{} exists but is not a directory", path.display());
        }
        let uid = own_uid();
        if md.uid() != uid {
            anyhow::bail!("{} is owned by uid {}, not {uid}", path.display(), md.uid());
        }
        if md.mode() & 0o077 != 0 {
            anyhow::bail!(
                "{} is group/other accessible (mode {:o})",
                path.display(),
                md.mode() & 0o7777
            );
        }
        Ok(())
    }

    /// Private per-uid directory holding the heartbeat socket and the
    /// clean-shutdown flag.
    ///
    /// `$XDG_RUNTIME_DIR/buffr` when usable (that tree is already 0700 and
    /// per-uid), otherwise `$TMPDIR/buffr-<uid>` created 0700. Both are
    /// verified after creation — see [`ensure_private_dir`].
    fn private_runtime_dir() -> anyhow::Result<PathBuf> {
        if let Some(xdg) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
            let p = PathBuf::from(xdg).join("buffr");
            match ensure_private_dir(&p) {
                Ok(()) => return Ok(p),
                Err(e) => {
                    tracing::warn!(
                        path = %p.display(),
                        error = %e,
                        "supervisor: XDG_RUNTIME_DIR unusable; falling back to temp dir"
                    );
                }
            }
        }
        let p = std::env::temp_dir().join(format!("buffr-{}", own_uid()));
        ensure_private_dir(&p)?;
        Ok(p)
    }

    /// Is the clean-shutdown flag genuinely present and ours?
    ///
    /// `Path::exists()` follows symlinks and does not check ownership, so a
    /// pre-planted symlink (or, before the private-directory change, a file
    /// created by any local user) would permanently suppress crash restarts.
    fn clean_flag_present(path: &Path) -> bool {
        match std::fs::symlink_metadata(path) {
            Ok(md) if !md.is_file() => {
                tracing::warn!(
                    path = %path.display(),
                    "supervisor: clean-shutdown flag is not a regular file; ignoring"
                );
                false
            }
            Ok(md) if md.uid() != own_uid() => {
                tracing::warn!(
                    path = %path.display(),
                    owner_uid = md.uid(),
                    "supervisor: clean-shutdown flag not owned by us; ignoring"
                );
                false
            }
            Ok(_) => true,
            Err(_) => false,
        }
    }

    // ── Supervisor loop ──────────────────────────────────────────────────────

    pub fn run_supervisor(
        child_bin: PathBuf,
        child_args: Vec<OsString>,
        heartbeat_timeout: Duration,
        heartbeat_disable: bool,
    ) -> anyhow::Result<()> {
        let mut crashes = CrashWindow::new(Duration::from_secs(WINDOW_SECS), CRASH_LIMIT);
        let mut restart_count: u32 = 0;

        // Flag set when supervisor receives SIGINT/SIGTERM so we know
        // a forwarded-signal exit from the child is intentional.
        let shutdown_requested = Arc::new(AtomicBool::new(false));

        // Our own PID used for the socket path.
        let supervisor_pid = std::process::id();

        let runtime_dir = match private_runtime_dir() {
            Ok(d) => Some(d),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "supervisor: no private runtime directory; heartbeat and \
                     clean-shutdown detection are disabled"
                );
                None
            }
        };

        // Clean-shutdown flag path. Deliberately independent of the heartbeat
        // socket: it is what stops a segfault during CEF/wgpu teardown (after
        // the user closed the window) from being read as a crash, and that
        // matters just as much under `--heartbeat-disable` or after a bind
        // failure.
        let clean_flag_path = runtime_dir
            .as_ref()
            .map(|d| d.join(format!("buffr-{supervisor_pid}.clean")));

        // Install signal forwarding exactly ONCE. The handler thread blocks
        // in `signals.forever()` for the lifetime of the process, so calling
        // this per iteration leaked one thread per restart, each holding a
        // stale child pid that it would later `killpg` — a reused pid then
        // gets an unrelated process group killed.
        let child_pid_slot = Arc::new(AtomicI32::new(0));
        let _signal_guard =
            install_signal_forwarding(Arc::clone(&child_pid_slot), Arc::clone(&shutdown_requested));

        loop {
            // §11-7: a shutdown signal can land while we sleep the restart
            // cooldown below; don't spawn a fresh child after the user
            // asked to quit. The handler stays armed (see
            // install_signal_forwarding), so a second signal is handled
            // rather than hitting the default disposition and orphaning
            // the new child.
            if shutdown_requested.load(Ordering::SeqCst) {
                tracing::info!("shutdown requested during restart cooldown — not restarting");
                return Ok(());
            }
            // ── bind socket for this spawn ─────────────────────────────────
            let (sock_path, listener, hb_chan) = match (heartbeat_disable, runtime_dir.as_ref()) {
                (false, Some(dir)) => match setup_heartbeat_socket(dir, supervisor_pid) {
                    Ok((path, listener)) => {
                        let (tx, rx) = std::sync::mpsc::channel::<HeartbeatEvent>();
                        (Some(path), Some(listener), Some((tx, rx)))
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "heartbeat: failed to bind socket; running without hang detection"
                        );
                        (None, None, None)
                    }
                },
                _ => (None, None, None),
            };

            // Remove any stale flag from a prior spawn so it cannot suppress
            // a real crash restart.
            if let Some(ref p) = clean_flag_path {
                let _ = std::fs::remove_file(p);
            }

            // ── spawn child ───────────────────────────────────────────────
            let spawn_time = Instant::now();
            let mut cmd = build_command(
                &child_bin,
                &child_args,
                sock_path.as_deref(),
                clean_flag_path.as_deref(),
            );
            let child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => {
                    anyhow::bail!("failed to spawn child {}: {e}", child_bin.display());
                }
            };
            let mut child = UnixChild::new(child);
            let child_pid = child.pid();
            child_pid_slot.store(child_pid as i32, Ordering::SeqCst);
            tracing::info!(pid = child_pid, "child spawned");

            // ── start heartbeat listener thread ───────────────────────────
            let hb_cancel = Arc::new(AtomicBool::new(false));
            let (hb_rx, hb_join) = match hb_chan {
                Some((tx, rx)) => {
                    let listener = listener.expect("listener present when channel present");
                    let cancel = Arc::clone(&hb_cancel);
                    let accept_deadline = Instant::now() + connect_grace() + ACCEPT_DEADLINE_SLACK;
                    let join = std::thread::spawn(move || {
                        heartbeat_accept_loop(listener, tx, cancel, accept_deadline)
                    });
                    (Some(rx), Some(join))
                }
                None => (None, None),
            };

            // ── wait for heartbeat connect within grace window ────────────
            //
            // While waiting we also poll the child so a fast-exiting child
            // (e.g. /bin/true, --help, short subcommand) is detected before
            // the grace window expires and is NOT treated as a connect-
            // timeout "crash".
            let outcome = if let Some(ref rx) = hb_rx {
                match wait_for_connect(rx, &mut child, connect_grace()) {
                    ConnectResult::Connected => {
                        tracing::info!(pid = child_pid, "child connected to heartbeat socket");
                        // Post-connect grace: give CEF time to initialise.
                        let first_deadline =
                            Instant::now() + POST_CONNECT_GRACE + heartbeat_timeout;
                        watch_heartbeat(rx, &mut child, first_deadline, heartbeat_timeout)
                    }
                    ConnectResult::TimedOut => {
                        tracing::warn!(
                            pid = child_pid,
                            grace_ms = connect_grace().as_millis(),
                            "child did not connect to heartbeat socket within the grace \
                             window; treating as crash"
                        );
                        child.kill_and_reap();
                        WatchOutcome::Hang
                    }
                    ConnectResult::ChildExited(info) => WatchOutcome::Exited(info),
                }
            } else {
                // Heartbeat disabled or socket setup failed — just wait.
                WatchOutcome::Exited(child.wait_exit())
            };

            // ── stop the accept thread and release its fd (M7) ────────────
            hb_cancel.store(true, Ordering::SeqCst);
            drop(hb_rx);
            if let Some(join) = hb_join {
                let _ = join.join();
            }
            child_pid_slot.store(0, Ordering::SeqCst);

            let (hang_detected, exit) = match outcome {
                WatchOutcome::Hang => (true, None),
                WatchOutcome::Exited(info) => (false, Some(info)),
            };
            let elapsed = spawn_time.elapsed();

            // ── clean up socket ───────────────────────────────────────────
            if let Some(ref path) = sock_path {
                let _ = std::fs::remove_file(path);
            }

            // ── decide whether to restart ─────────────────────────────────
            let flag_present = clean_flag_path.as_deref().is_some_and(clean_flag_present);
            // Remove the flag eagerly — whether or not we restart, the
            // next spawn re-creates its own.
            if let Some(ref p) = clean_flag_path {
                let _ = std::fs::remove_file(p);
            }

            match classify(hang_detected, exit.as_ref(), flag_present) {
                Disposition::Done => {
                    tracing::info!(
                        pid = child_pid,
                        elapsed_ms = elapsed.as_millis(),
                        flag_present,
                        "child exited cleanly; supervisor done"
                    );
                    return Ok(());
                }
                Disposition::Propagate(code) => {
                    // If the supervisor itself was asked to shut down, the
                    // child's exit is intentional — don't propagate a failure.
                    if shutdown_requested.load(Ordering::SeqCst) {
                        tracing::info!(
                            pid = child_pid,
                            "child exited after supervisor shutdown signal; not restarting"
                        );
                        return Ok(());
                    }
                    tracing::info!(
                        pid = child_pid,
                        elapsed_ms = elapsed.as_millis(),
                        exit_code = code,
                        "child exited normally with non-zero code; not restarting \
                         (likely CLI or config error)"
                    );
                    std::process::exit(code);
                }
                Disposition::Restart => {}
            }

            if shutdown_requested.load(Ordering::SeqCst) {
                tracing::info!(
                    pid = child_pid,
                    "child exited after supervisor shutdown signal; not restarting"
                );
                return Ok(());
            }

            // Crash / hang path.
            restart_count += 1;
            let in_window = crashes.record(Instant::now());

            if hang_detected {
                tracing::info!(
                    pid = child_pid,
                    restart_count,
                    crashes_in_window = in_window,
                    elapsed_ms = elapsed.as_millis(),
                    "child hang detected; considering restart"
                );
            } else {
                tracing::info!(
                    pid = child_pid,
                    exit = ?exit,
                    restart_count,
                    crashes_in_window = in_window,
                    elapsed_ms = elapsed.as_millis(),
                    "child crashed; considering restart"
                );
            }

            if crashes.limit_reached() {
                tracing::error!(
                    "watchdog: {CRASH_LIMIT} crashes/hangs in {WINDOW_SECS}s, \
                     refusing to restart. Run buffr-app directly to capture \
                     its stderr (the supervisor does not redirect it yet): \
                     `RUST_LOG=debug buffr-app 2>buffr-app.log`"
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

    /// Bind a Unix-domain socket the child will connect to, inside the
    /// caller-supplied private runtime directory.
    ///
    /// Path: `<runtime_dir>/buffr-<pid>.sock`.
    ///
    /// Unlinks any stale socket at that path before binding — safe because
    /// the directory is 0700 and per-uid, so only we could have created it.
    /// Sets permissions to 0600 (owner-only).
    pub fn setup_heartbeat_socket(
        runtime_dir: &Path,
        pid: u32,
    ) -> anyhow::Result<(PathBuf, UnixListener)> {
        let path = runtime_dir.join(format!("buffr-{pid}.sock"));

        // Remove stale socket from a prior crash.
        if std::fs::symlink_metadata(&path).is_ok() {
            let _ = std::fs::remove_file(&path);
        }

        let listener = UnixListener::bind(&path)?;
        // Owner-only: the child runs as the same user; world-read is unnecessary.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        // Set non-blocking so our accept() call can time out.
        listener.set_nonblocking(true)?;

        tracing::debug!(path = %path.display(), "heartbeat socket bound");
        Ok((path, listener))
    }

    /// Accept exactly one connection; send events to `tx` until disconnected.
    ///
    /// Terminates on `cancel`, on `accept_deadline`, when the receiver goes
    /// away, or when the connection ends. Without those exits the thread
    /// polled `accept()` at 20 Hz forever after a connect timeout, leaking a
    /// thread and the listener fd on every restart.
    fn heartbeat_accept_loop(
        listener: UnixListener,
        tx: std::sync::mpsc::Sender<HeartbeatEvent>,
        cancel: Arc<AtomicBool>,
        accept_deadline: Instant,
    ) {
        use std::io::Read;

        // Poll for a client (the listener is non-blocking).
        let stream = loop {
            if cancel.load(Ordering::SeqCst) {
                tracing::debug!("heartbeat: accept loop cancelled before connect");
                return;
            }
            if Instant::now() >= accept_deadline {
                tracing::debug!("heartbeat: accept deadline expired; listener released");
                return;
            }
            match listener.accept() {
                Ok((s, _)) => break s,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "heartbeat: accept failed");
                    return;
                }
            }
        };

        // Notify main thread that a connection arrived.
        if tx.send(HeartbeatEvent::Connected).is_err() {
            return;
        }

        // Read bytes; each byte is a ping.
        let mut stream = stream;
        stream
            .set_read_timeout(Some(Duration::from_millis(500)))
            .ok();
        let mut buf = [0u8; 64];
        loop {
            if cancel.load(Ordering::SeqCst) {
                return;
            }
            match stream.read(&mut buf) {
                Ok(0) => {
                    // EOF — child closed the socket.
                    let _ = tx.send(HeartbeatEvent::Disconnected);
                    return;
                }
                Ok(n) => {
                    for _ in 0..n {
                        if tx.send(HeartbeatEvent::Ping).is_err() {
                            return;
                        }
                    }
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    // Timeout — no ping yet; loop back so the main thread
                    // can detect a hang without us blocking forever.
                    continue;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "heartbeat: read error");
                    let _ = tx.send(HeartbeatEvent::Disconnected);
                    return;
                }
            }
        }
    }

    fn build_command(
        bin: &PathBuf,
        args: &[OsString],
        sock_path: Option<&std::path::Path>,
        clean_flag_path: Option<&std::path::Path>,
    ) -> std::process::Command {
        use std::os::unix::process::CommandExt;

        let mut cmd = std::process::Command::new(bin);
        cmd.args(args);

        // Pass the socket path to the child via env var (if heartbeat active).
        if let Some(path) = sock_path {
            cmd.env(SUPERVISOR_SOCK_ENV, path);
        } else {
            // A stale value inherited from our own environment would make the
            // child try to connect to somebody else's socket.
            cmd.env_remove(SUPERVISOR_SOCK_ENV);
        }

        // Pass the clean-shutdown flag path so the child can signal
        // intentional close. Supervisor reads it on exit; presence
        // overrides exit-status crash detection.
        if let Some(path) = clean_flag_path {
            cmd.env(SUPERVISOR_CLEAN_FLAG_ENV, path);
        } else {
            cmd.env_remove(SUPERVISOR_CLEAN_FLAG_ENV);
        }

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

    /// Install handlers for SIGINT and SIGTERM, once for the whole process.
    ///
    /// The handler stays armed for the supervisor's lifetime, handling every
    /// signal (previously it unregistered after the first one). On receipt,
    /// forward the signal to the *current* child's process group via `killpg`,
    /// wait up to `GRACEFUL_TIMEOUT` for it to exit, then SIGKILL if still
    /// alive. Sets `shutdown_requested` so the main loop knows not to restart
    /// after the forwarded signal.
    ///
    /// The child pid is read from `child_pid_slot` at signal time rather than
    /// captured at install time: the supervisor restarts the child, and a
    /// handler holding a stale (possibly reused) pid would signal an
    /// unrelated process group.
    fn install_signal_forwarding(
        child_pid_slot: Arc<AtomicI32>,
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

            // §11-7: stay armed. The old single-shot handler unregistered
            // after the first signal, so a second Ctrl+C (e.g. while a
            // fresh child was up after a restart-cooldown signal) hit the
            // default disposition, killing the supervisor and orphaning
            // the child. Re-arm by looping.
            for sig in signals.forever() {
                tracing::info!(
                    signal = sig,
                    "supervisor received signal; forwarding to child pgrp"
                );
                shutdown_requested.store(true, Ordering::SeqCst);

                let raw = child_pid_slot.load(Ordering::SeqCst);
                if raw <= 0 {
                    // No live child to forward to (e.g. mid-restart
                    // cooldown) — the main loop's shutdown checks handle
                    // exit. Stay armed for the next signal.
                    tracing::info!("no live child to forward the signal to");
                    continue;
                }
                // child_pid IS the pgid since the child called setsid().
                let child_pid = Pid::from_raw(raw);
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

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn ensure_private_dir_creates_0700_and_accepts_it() {
            let base = tempfile::tempdir().expect("tempdir");
            let p = base.path().join("buffr-runtime");
            ensure_private_dir(&p).expect("first create");
            let md = std::fs::symlink_metadata(&p).unwrap();
            assert_eq!(md.mode() & 0o7777, 0o700, "expected 0700");
            // Idempotent.
            ensure_private_dir(&p).expect("second call");
        }

        #[test]
        fn ensure_private_dir_rejects_group_writable() {
            let base = tempfile::tempdir().expect("tempdir");
            let p = base.path().join("loose");
            std::fs::create_dir(&p).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o777)).unwrap();
            let err = ensure_private_dir(&p).expect_err("0777 dir must be rejected");
            assert!(
                err.to_string().contains("group/other"),
                "unexpected error: {err}"
            );
        }

        #[test]
        fn ensure_private_dir_rejects_symlink() {
            let base = tempfile::tempdir().expect("tempdir");
            let target = base.path().join("real");
            std::fs::create_dir(&target).unwrap();
            let link = base.path().join("link");
            std::os::unix::fs::symlink(&target, &link).unwrap();
            let err = ensure_private_dir(&link).expect_err("symlink must be rejected");
            assert!(
                err.to_string().contains("not a directory"),
                "unexpected error: {err}"
            );
        }

        #[test]
        fn clean_flag_present_requires_a_regular_file_we_own() {
            let base = tempfile::tempdir().expect("tempdir");
            let missing = base.path().join("nope.clean");
            assert!(!clean_flag_present(&missing), "absent flag must be false");

            let real = base.path().join("yes.clean");
            std::fs::write(&real, b"").unwrap();
            assert!(clean_flag_present(&real), "our own regular file counts");

            // A symlink pointing at a file we own must NOT count: the
            // symlink itself is what an attacker plants.
            let link = base.path().join("link.clean");
            std::os::unix::fs::symlink(&real, &link).unwrap();
            assert!(
                !clean_flag_present(&link),
                "symlinked flag must be rejected (Path::exists would follow it)"
            );

            // A directory is not a flag either.
            let dir = base.path().join("dir.clean");
            std::fs::create_dir(&dir).unwrap();
            assert!(!clean_flag_present(&dir), "directory must be rejected");
        }
    }
}

// ── Windows supervisor (Job Objects + named-pipe heartbeat) ─────────────────
#[cfg(windows)]
mod windows {
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{
        CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_INBOUND;
    use windows_sys::Win32::System::Console::{
        CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, SetConsoleCtrlHandler,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject,
    };
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_TYPE_BYTE,
        PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };
    use windows_sys::Win32::System::Threading::{
        CREATE_BREAKAWAY_FROM_JOB, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
        GetExitCodeProcess, INFINITE, PROCESS_INFORMATION, ResumeThread, STARTUPINFOW,
        TerminateProcess, WaitForSingleObject,
    };

    use crate::supervisor::{
        ChildHandle, ConnectResult, CrashWindow, Disposition, ExitInfo, HeartbeatEvent,
        WatchOutcome, build_command_line, classify, is_crash_exit_code, wait_for_connect,
        watch_heartbeat,
    };

    /// Rolling window for crash backoff detection.
    const WINDOW_SECS: u64 = 30;
    const CRASH_LIMIT: usize = 3;
    /// Restart cooldown between attempts.
    const RESTART_COOLDOWN: Duration = Duration::from_millis(250);
    /// How long to wait for the child to connect to the named pipe. Sized
    /// for cold-disk first runs (scoop / MSI install on a fresh Windows
    /// machine) where CEF library load + SQLite db opens can dominate
    /// startup. 5 s was too tight under those conditions. Integration
    /// tests override via `BUFFR_CONNECT_GRACE_MS` to keep runtime bounded.
    const DEFAULT_CONNECT_GRACE: Duration = Duration::from_secs(20);

    fn connect_grace() -> Duration {
        std::env::var("BUFFR_CONNECT_GRACE_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_CONNECT_GRACE)
    }
    /// Additional grace after the child connects before enforcing heartbeat.
    const POST_CONNECT_GRACE: Duration = Duration::from_millis(1500);

    /// Env var passed to the child with the named-pipe path.
    pub const SUPERVISOR_PIPE_ENV: &str = "BUFFR_SUPERVISOR_PIPE";
    /// Env var passed to the child with the clean-shutdown flag path.
    /// Mirrors the Unix side: the child touches this file before an
    /// intentional exit so a segfault during CEF/wgpu teardown after the
    /// user closed the window is not mistaken for a crash.
    pub const SUPERVISOR_CLEAN_FLAG_ENV: &str = "BUFFR_SUPERVISOR_CLEAN_FLAG";

    /// Named pipe path: `\\.\pipe\buffr-supervisor-<pid>`.
    fn pipe_name(pid: u32) -> Vec<u16> {
        let name = format!("\\\\.\\pipe\\buffr-supervisor-{pid}\0");
        name.encode_utf16().collect()
    }

    /// HANDLE → usize for comparisons that work on both MSVC and GNU ABIs.
    ///
    /// On MSVC, HANDLE is isize; on GNU, HANDLE is *mut c_void.  Casting
    /// through usize is safe for pointer-sized values on both.
    fn handle_as_usize(h: HANDLE) -> usize {
        h as usize
    }

    fn null_handle() -> usize {
        0usize
    }

    /// A raw Win32 HANDLE wrapper that closes on drop.
    struct OwnedHandle(HANDLE);

    impl OwnedHandle {
        fn raw(&self) -> HANDLE {
            self.0
        }
    }

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            let h = handle_as_usize(self.0);
            let inv = handle_as_usize(INVALID_HANDLE_VALUE);
            if h != null_handle() && h != inv {
                // SAFETY: handle is valid and owned exclusively by this struct.
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    // SAFETY: HANDLE is a pointer-sized kernel object reference; we never
    // share ownership of the underlying object across threads without
    // synchronisation.
    unsafe impl Send for OwnedHandle {}

    // ── Child handle ─────────────────────────────────────────────────────────

    /// A spawned `buffr-app.exe`, exposed to the shared supervisor logic
    /// through [`ChildHandle`].
    ///
    /// The process handle is owned here and closed on drop.
    struct WindowsChild {
        handle: OwnedHandle,
        pid: u32,
        exit: Option<ExitInfo>,
    }

    impl WindowsChild {
        fn new(handle: HANDLE, pid: u32) -> Self {
            Self {
                handle: OwnedHandle(handle),
                pid,
                exit: None,
            }
        }

        fn raw(&self) -> HANDLE {
            self.handle.raw()
        }

        /// Read the exit code of a process already known to have exited.
        fn harvest(&mut self) -> ExitInfo {
            let mut code: u32 = 0;
            // SAFETY: handle is valid; code is initialised.
            let ok = unsafe { GetExitCodeProcess(self.raw(), std::ptr::addr_of_mut!(code)) };
            let info = if ok != 0 {
                ExitInfo {
                    code: Some(code as i32),
                    crashed: is_crash_exit_code(code),
                }
            } else {
                tracing::warn!(
                    error = %std::io::Error::last_os_error(),
                    "GetExitCodeProcess failed; treating as crash"
                );
                ExitInfo {
                    code: None,
                    crashed: true,
                }
            };
            self.exit = Some(info);
            info
        }
    }

    impl ChildHandle for WindowsChild {
        fn pid(&self) -> u32 {
            self.pid
        }

        fn poll_exit(&mut self) -> Option<ExitInfo> {
            if let Some(e) = self.exit {
                return Some(e);
            }
            // GetExitCodeProcess on a live process succeeds and yields
            // STILL_ACTIVE (259) — indistinguishable from a real exit code
            // of 259. Gate it on the process actually being signalled.
            // SAFETY: handle is valid; 0 timeout → non-blocking poll.
            let r = unsafe { WaitForSingleObject(self.raw(), 0) };
            if r == WAIT_OBJECT_0 {
                Some(self.harvest())
            } else {
                None
            }
        }

        fn wait_exit(&mut self) -> ExitInfo {
            if let Some(e) = self.exit {
                return e;
            }
            // SAFETY: handle is valid; INFINITE is a documented sentinel.
            unsafe { WaitForSingleObject(self.raw(), INFINITE) };
            self.harvest()
        }

        fn kill_and_reap(&mut self) -> ExitInfo {
            if let Some(e) = self.exit {
                return e;
            }
            // SAFETY: handle is valid; exit code 1 signals abnormal termination.
            unsafe { TerminateProcess(self.raw(), 1) };
            self.wait_exit()
        }
    }

    // ── Clean-shutdown flag ──────────────────────────────────────────────────

    /// Directory holding the clean-shutdown flag.
    ///
    /// `%TEMP%` on Windows is already per-user (`%LOCALAPPDATA%\Temp`), which
    /// is the equivalent of the Unix side's 0700 per-uid directory; we add a
    /// `buffr` subdirectory so the files are grouped.
    fn clean_flag_dir() -> anyhow::Result<PathBuf> {
        let dir = std::env::temp_dir().join("buffr");
        std::fs::create_dir_all(&dir)
            .map_err(|e| anyhow::anyhow!("creating {}: {e}", dir.display()))?;
        Ok(dir)
    }

    /// Is the clean-shutdown flag genuinely present?
    ///
    /// `symlink_metadata` + `is_file()` so a directory junction or symlink
    /// planted at the path is not honoured.
    fn clean_flag_present(path: &Path) -> bool {
        match std::fs::symlink_metadata(path) {
            Ok(md) if md.is_file() => true,
            Ok(_) => {
                tracing::warn!(
                    path = %path.display(),
                    "supervisor: clean-shutdown flag is not a regular file; ignoring"
                );
                false
            }
            Err(_) => false,
        }
    }

    // ── Supervisor loop ──────────────────────────────────────────────────────

    pub fn run_supervisor(
        child_bin: PathBuf,
        child_args: Vec<OsString>,
        heartbeat_timeout: Duration,
        heartbeat_disable: bool,
    ) -> anyhow::Result<()> {
        let mut crashes = CrashWindow::new(Duration::from_secs(WINDOW_SECS), CRASH_LIMIT);
        let mut restart_count: u32 = 0;

        let shutdown_requested = Arc::new(AtomicBool::new(false));
        let supervisor_pid = std::process::id();

        // ── Global Job Object ─────────────────────────────────────────────────
        // One job per supervisor lifetime. All child spawns are assigned to
        // this job so KILL_ON_JOB_CLOSE terminates the entire tree when the
        // supervisor exits.
        let job = create_job_object()?;

        // Install Ctrl+C / Ctrl+Break / close handler.
        install_ctrl_handler(Arc::clone(&shutdown_requested), job.raw());

        // Clean-shutdown flag path, computed independently of the heartbeat
        // pipe so `--heartbeat-disable` (or a pipe failure) does not silently
        // turn a clean teardown segfault into a restart loop.
        let clean_flag_path = match clean_flag_dir() {
            Ok(d) => Some(d.join(format!("buffr-{supervisor_pid}.clean"))),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "supervisor: no clean-flag directory; clean-shutdown detection disabled"
                );
                None
            }
        };

        loop {
            // ── bind named pipe for this spawn ────────────────────────────────
            let (pipe_path_str, pipe_handle, hb_rx) = if heartbeat_disable {
                (None, None, None)
            } else {
                match create_heartbeat_pipe(supervisor_pid) {
                    Ok((path, handle)) => {
                        let (tx, rx) = std::sync::mpsc::channel::<HeartbeatEvent>();
                        (Some(path), Some(handle), Some((tx, rx)))
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "heartbeat: failed to create named pipe; running without hang detection"
                        );
                        (None, None, None)
                    }
                }
            };

            // Remove any stale flag from a prior spawn.
            if let Some(ref p) = clean_flag_path {
                let _ = std::fs::remove_file(p);
            }

            // ── spawn child (suspended) + assign to job ───────────────────────
            let spawn_time = Instant::now();
            let proc_info = match spawn_child_suspended(
                &child_bin,
                &child_args,
                pipe_path_str.as_deref(),
                clean_flag_path.as_deref(),
            ) {
                Ok(pi) => pi,
                Err(e) => {
                    anyhow::bail!("failed to spawn child {}: {e}", child_bin.display());
                }
            };

            let mut child = WindowsChild::new(proc_info.hProcess, proc_info.dwProcessId);
            let child_pid = child.pid();
            tracing::info!(pid = child_pid, "child spawned (suspended)");

            // ── assign-before-resume (critical ordering) ──────────────────────
            // SAFETY: job and process handles are valid. Assign before
            // ResumeThread so any descendants the child spawns after resume
            // also land in the job automatically.
            let assign_ok = unsafe { AssignProcessToJobObject(job.raw(), child.raw()) != 0 };
            if !assign_ok {
                let err = std::io::Error::last_os_error();
                // Non-fatal if the process is already in a job (Windows 8+
                // allows nested jobs). Log and continue.
                tracing::warn!(error = %err, "AssignProcessToJobObject failed; continuing");
            }

            // ── resume main thread ────────────────────────────────────────────
            // SAFETY: thread handle is valid; we are the only caller of
            // ResumeThread for this handle.
            let prev = unsafe { ResumeThread(proc_info.hThread) };
            if prev == u32::MAX {
                let err = std::io::Error::last_os_error();
                tracing::warn!(error = %err, "ResumeThread failed");
            }
            // SAFETY: thread handle is valid; drop it now — we no longer need it.
            unsafe { CloseHandle(proc_info.hThread) };

            // ── start heartbeat listener thread ───────────────────────────────
            let hb_rx = if let (Some(ph), Some((tx, rx))) = (pipe_handle, hb_rx) {
                std::thread::spawn(move || heartbeat_pipe_loop(ph, tx));
                Some(rx)
            } else {
                None
            };

            // ── wait for child + heartbeat ────────────────────────────────────
            let outcome = if let Some(ref rx) = hb_rx {
                match wait_for_connect(rx, &mut child, connect_grace()) {
                    ConnectResult::Connected => {
                        tracing::info!(pid = child_pid, "child connected to heartbeat pipe");
                        let first_deadline =
                            Instant::now() + POST_CONNECT_GRACE + heartbeat_timeout;
                        watch_heartbeat(rx, &mut child, first_deadline, heartbeat_timeout)
                    }
                    ConnectResult::TimedOut => {
                        tracing::warn!(
                            pid = child_pid,
                            grace_ms = connect_grace().as_millis(),
                            "child did not connect to heartbeat pipe within the grace \
                             window; treating as crash"
                        );
                        child.kill_and_reap();
                        WatchOutcome::Hang
                    }
                    ConnectResult::ChildExited(info) => WatchOutcome::Exited(info),
                }
            } else {
                // Heartbeat disabled — block until child exits.
                WatchOutcome::Exited(child.wait_exit())
            };
            drop(hb_rx);

            let (hang_detected, exit) = match outcome {
                WatchOutcome::Hang => (true, None),
                WatchOutcome::Exited(info) => (false, Some(info)),
            };
            // Releases the process handle.
            drop(child);

            let elapsed = spawn_time.elapsed();

            // ── decide whether to restart ─────────────────────────────────────
            let flag_present = clean_flag_path.as_deref().is_some_and(clean_flag_present);
            if let Some(ref p) = clean_flag_path {
                let _ = std::fs::remove_file(p);
            }

            match classify(hang_detected, exit.as_ref(), flag_present) {
                Disposition::Done => {
                    tracing::info!(
                        pid = child_pid,
                        elapsed_ms = elapsed.as_millis(),
                        flag_present,
                        "child exited cleanly; supervisor done"
                    );
                    return Ok(());
                }
                Disposition::Propagate(code) => {
                    if shutdown_requested.load(Ordering::SeqCst) {
                        tracing::info!(
                            pid = child_pid,
                            "child exited after shutdown signal; not restarting"
                        );
                        return Ok(());
                    }
                    tracing::info!(
                        pid = child_pid,
                        elapsed_ms = elapsed.as_millis(),
                        exit_code = code,
                        "child exited normally with non-zero code; not restarting \
                         (likely CLI or config error)"
                    );
                    std::process::exit(code);
                }
                Disposition::Restart => {}
            }

            if shutdown_requested.load(Ordering::SeqCst) {
                tracing::info!(
                    pid = child_pid,
                    "child exited after shutdown signal; not restarting"
                );
                return Ok(());
            }

            // Crash / hang path.
            restart_count += 1;
            let in_window = crashes.record(Instant::now());

            if hang_detected {
                tracing::info!(
                    pid = child_pid,
                    restart_count,
                    crashes_in_window = in_window,
                    elapsed_ms = elapsed.as_millis(),
                    "child hang detected; considering restart"
                );
            } else {
                tracing::info!(
                    pid = child_pid,
                    exit = ?exit,
                    restart_count,
                    crashes_in_window = in_window,
                    elapsed_ms = elapsed.as_millis(),
                    "child crashed; considering restart"
                );
            }

            if crashes.limit_reached() {
                tracing::error!(
                    "watchdog: {CRASH_LIMIT} crashes/hangs in {WINDOW_SECS}s, \
                     refusing to restart. Run buffr-app directly from a \
                     PowerShell prompt to capture its stderr (the supervisor \
                     does not redirect it yet): \
                     `$env:RUST_LOG=\"debug\"; buffr-app 2>buffr-app.log`"
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

    // ── Job Object ────────────────────────────────────────────────────────────

    fn create_job_object() -> anyhow::Result<OwnedHandle> {
        // SAFETY: NULL name → anonymous job; NULL security attributes → defaults.
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle_as_usize(job) == null_handle() {
            anyhow::bail!(
                "CreateJobObjectW failed: {}",
                std::io::Error::last_os_error()
            );
        }

        // Enable KILL_ON_JOB_CLOSE so the entire child tree is terminated when
        // the job handle is closed (i.e. when the supervisor exits for any reason).
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION =
            // SAFETY: zero-initialising a POD struct.
            unsafe { std::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

        // SAFETY: job handle is valid; info is a correctly-sized struct for
        // JobObjectExtendedLimitInformation.
        let ok = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                std::ptr::addr_of!(info).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            // SAFETY: job handle is valid; we're bailing so the handle leaks,
            // but the process is about to exit.
            unsafe { CloseHandle(job) };
            anyhow::bail!(
                "SetInformationJobObject failed: {}",
                std::io::Error::last_os_error()
            );
        }

        Ok(OwnedHandle(job))
    }

    // ── Ctrl handler ─────────────────────────────────────────────────────────

    /// Per-process singleton for the Ctrl handler callback.
    ///
    /// Stored as `usize` so the atomic works on both MSVC (HANDLE = isize)
    /// and GNU (HANDLE = *mut c_void) ABIs without mismatched-type errors.
    static JOB_HANDLE_FOR_CTRL: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);
    static SHUTDOWN_FLAG_FOR_CTRL: std::sync::OnceLock<Arc<AtomicBool>> =
        std::sync::OnceLock::new();

    fn install_ctrl_handler(shutdown: Arc<AtomicBool>, job: HANDLE) {
        // Store the job handle and flag in the statics so the callback can
        // reach them (Win32 ctrl handlers are bare fn pointers).
        JOB_HANDLE_FOR_CTRL.store(handle_as_usize(job), Ordering::SeqCst);
        let _ = SHUTDOWN_FLAG_FOR_CTRL.set(shutdown);

        // SAFETY: ctrl_handler satisfies the PHANDLER_ROUTINE signature;
        // TRUE (1) → we are adding (not removing) the handler.
        unsafe { SetConsoleCtrlHandler(Some(ctrl_handler), 1) };
    }

    /// Called by Windows on Ctrl+C, Ctrl+Break, or console window close.
    ///
    /// Terminates the entire job (killing all child processes) then returns
    /// FALSE so the default handler also runs (which exits the process).
    ///
    /// # Safety
    /// Called from a dedicated Windows console-event thread. The only
    /// shared state accessed is the atomic job handle (usize) and the
    /// OnceLock flag, both of which are safe to read from any thread.
    unsafe extern "system" fn ctrl_handler(ctrl_type: u32) -> i32 {
        if matches!(
            ctrl_type,
            CTRL_C_EVENT | CTRL_BREAK_EVENT | CTRL_CLOSE_EVENT
        ) {
            if let Some(flag) = SHUTDOWN_FLAG_FOR_CTRL.get() {
                flag.store(true, Ordering::SeqCst);
            }
            let raw = JOB_HANDLE_FOR_CTRL.load(Ordering::SeqCst);
            let inv = handle_as_usize(INVALID_HANDLE_VALUE);
            if raw != null_handle() && raw != inv {
                // Cast back to HANDLE for the Win32 call.
                // SAFETY: raw was stored from a valid HANDLE cast through usize;
                // the round-trip is lossless on all Windows pointer-sized types.
                let job = raw as HANDLE;
                // SAFETY: job handle is valid; exit code 0 signals a clean stop.
                unsafe { TerminateJobObject(job, 0) };
            }
        }
        // Return FALSE (0) → default handler (ExitProcess) runs next.
        0
    }

    // ── Named pipe setup ──────────────────────────────────────────────────────

    /// Create the inbound named pipe the child will connect to.
    /// Returns the pipe path string (UTF-8) and the pipe handle.
    fn create_heartbeat_pipe(pid: u32) -> anyhow::Result<(String, OwnedHandle)> {
        let path_str = format!("\\\\.\\pipe\\buffr-supervisor-{pid}");
        let path_wide = pipe_name(pid);

        // SAFETY: path_wide is NUL-terminated wide string; other args are
        // documented constant values for a byte-mode inbound pipe.
        let handle = unsafe {
            CreateNamedPipeW(
                path_wide.as_ptr(),
                PIPE_ACCESS_INBOUND,
                PIPE_TYPE_BYTE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                0,    // out buf (not used — inbound only)
                4096, // in buf
                0,    // default timeout
                std::ptr::null(),
            )
        };

        if handle == INVALID_HANDLE_VALUE {
            anyhow::bail!(
                "CreateNamedPipeW failed: {}",
                std::io::Error::last_os_error()
            );
        }

        tracing::debug!(path = %path_str, "heartbeat named pipe created");
        Ok((path_str, OwnedHandle(handle)))
    }

    // ── Child spawn ───────────────────────────────────────────────────────────

    /// Build an explicit `CREATE_UNICODE_ENVIRONMENT` block for the child.
    ///
    /// The previous approach mutated the *supervisor's* own environment
    /// around `CreateProcessW`. That is a data race on any restart iteration,
    /// where the previous iteration's heartbeat thread is still live —
    /// `set_var`/`remove_var` are not thread-safe.
    ///
    /// Format: `KEY=VALUE\0KEY=VALUE\0\0`, sorted.
    fn build_environment_block(extra: &[(&str, OsString)]) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;

        let mut vars: BTreeMap<OsString, OsString> = std::env::vars_os().collect();
        for (k, v) in extra {
            vars.insert(OsString::from(*k), v.clone());
        }

        let mut block: Vec<u16> = Vec::new();
        for (k, v) in vars {
            if k.is_empty() {
                continue;
            }
            block.extend(k.encode_wide());
            block.push(u16::from(b'='));
            block.extend(v.encode_wide());
            block.push(0);
        }
        // An empty block still needs the leading NUL of the "empty string"
        // entry before the terminator.
        if block.is_empty() {
            block.push(0);
        }
        block.push(0);
        block
    }

    fn spawn_child_suspended(
        bin: &Path,
        args: &[OsString],
        pipe_path: Option<&str>,
        clean_flag_path: Option<&Path>,
    ) -> anyhow::Result<PROCESS_INFORMATION> {
        // Build a Windows command line with MSVCRT quoting so paths with
        // spaces survive and an embedded `"` cannot inject extra flags.
        let cmdline = build_command_line(bin, args);
        let mut cmdline_wide: Vec<u16> = cmdline.encode_utf16().chain([0]).collect();

        let mut extra: Vec<(&str, OsString)> = Vec::new();
        if let Some(path) = pipe_path {
            extra.push((SUPERVISOR_PIPE_ENV, OsString::from(path)));
        }
        if let Some(path) = clean_flag_path {
            extra.push((SUPERVISOR_CLEAN_FLAG_ENV, path.as_os_str().to_os_string()));
        }
        let env_block = build_environment_block(&extra);

        let mut si: STARTUPINFOW =
            // SAFETY: zero-initialising a POD struct.
            unsafe { std::mem::zeroed() };
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        let mut pi: PROCESS_INFORMATION =
            // SAFETY: zero-initialising a POD struct.
            unsafe { std::mem::zeroed() };

        // CREATE_SUSPENDED: child is created but not started — lets us assign
        // to the job before any code runs (including spawning CEF helpers).
        // CREATE_BREAKAWAY_FROM_JOB: needed if the supervisor is itself inside
        // a job (e.g. under VS Test runner or some CI systems) so the child
        // can then be placed into *our* job without nesting conflicts.
        // CREATE_UNICODE_ENVIRONMENT: lpEnvironment below is UTF-16.
        let flags = CREATE_SUSPENDED | CREATE_BREAKAWAY_FROM_JOB | CREATE_UNICODE_ENVIRONMENT;

        // SAFETY: cmdline_wide is NUL-terminated; env_block is a double-NUL
        // terminated UTF-16 environment block that outlives the call; si/pi
        // point to valid zeroed structs; NULL for application name uses the
        // cmdline parsing path.
        let ok = unsafe {
            CreateProcessW(
                std::ptr::null(),
                cmdline_wide.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0, // bInheritHandles = FALSE
                flags,
                env_block.as_ptr().cast(),
                std::ptr::null(),
                std::ptr::addr_of!(si),
                std::ptr::addr_of_mut!(pi),
            )
        };

        if ok == 0 {
            anyhow::bail!("CreateProcessW failed: {}", std::io::Error::last_os_error());
        }

        Ok(pi)
    }

    // ── Heartbeat pipe I/O ────────────────────────────────────────────────────

    /// Wait for the client to connect, then read ping bytes.
    fn heartbeat_pipe_loop(pipe: OwnedHandle, tx: std::sync::mpsc::Sender<HeartbeatEvent>) {
        // Block until the client connects.
        // SAFETY: pipe handle is valid and exclusively owned by this thread.
        // NULL overlapped → synchronous (blocking) ConnectNamedPipe.
        let connected = unsafe {
            ConnectNamedPipe(
                pipe.raw(),
                std::ptr::null_mut::<windows_sys::Win32::System::IO::OVERLAPPED>(),
            )
        };
        // ConnectNamedPipe returns 0 on error, non-zero on success.
        // ERROR_PIPE_CONNECTED (535) means client already connected before we called — also OK.
        if connected == 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(535) {
                tracing::warn!(error = %err, "heartbeat: ConnectNamedPipe failed");
                return;
            }
        }

        if tx.send(HeartbeatEvent::Connected).is_err() {
            return;
        }

        // Read bytes in a loop; each byte is a ping.
        let mut buf = [0u8; 64];
        loop {
            let mut bytes_read: u32 = 0;
            // SAFETY: pipe handle is valid; buf is a valid mutable slice;
            // NULL overlapped → synchronous read.
            let ok = unsafe {
                windows_sys::Win32::Storage::FileSystem::ReadFile(
                    pipe.raw(),
                    buf.as_mut_ptr().cast(),
                    buf.len() as u32,
                    std::ptr::addr_of_mut!(bytes_read),
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 || bytes_read == 0 {
                let _ = tx.send(HeartbeatEvent::Disconnected);
                // SAFETY: pipe handle is valid; we're done with it.
                unsafe { DisconnectNamedPipe(pipe.raw()) };
                return;
            }
            for _ in 0..bytes_read {
                if tx.send(HeartbeatEvent::Ping).is_err() {
                    return;
                }
            }
        }
    }
}

// ── Non-Unix/non-Windows fallback ────────────────────────────────────────────
#[cfg(not(any(unix, windows)))]
mod other {
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
