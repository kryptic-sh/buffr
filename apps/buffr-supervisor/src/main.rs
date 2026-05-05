//! buffr-supervisor — crash-restart + hang watchdog for the buffr browser binary.
//!
//! **Linux only** in Round 2. On other platforms the supervisor prints a
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

/// Crash-restart + hang watchdog for the buffr browser binary.
///
/// Forwards all arguments to the buffr child process and automatically
/// restarts it on crash or UI hang. Stops after 3 crashes/hangs in
/// 30 seconds and points at the crash log directory.
#[derive(Debug, Parser)]
#[command(
    name = "buffr-supervisor",
    version = env!("CARGO_PKG_VERSION"),
    about = "Crash-restart + hang watchdog for buffr. Forwards args to the buffr browser \
             binary and restarts on crash or hang. Linux only in this release.",
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
    let heartbeat_timeout = std::time::Duration::from_secs(cli.heartbeat_timeout);
    let heartbeat_disable = cli.heartbeat_disable;

    tracing::info!(
        child = %child_bin.display(),
        heartbeat_timeout_s = cli.heartbeat_timeout,
        heartbeat_disable,
        "buffr-supervisor starting"
    );

    #[cfg(target_os = "linux")]
    {
        linux::run_supervisor(child_bin, child_args, heartbeat_timeout, heartbeat_disable)?;
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
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;
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
    /// How long to wait for the child to connect to the heartbeat socket.
    const CONNECT_GRACE: Duration = Duration::from_secs(5);
    /// Additional grace after the child connects before enforcing heartbeat.
    const POST_CONNECT_GRACE: Duration = Duration::from_millis(1500);

    /// Env var name written into the child's environment with the UDS path.
    pub const SUPERVISOR_SOCK_ENV: &str = "BUFFR_SUPERVISOR_SOCK";

    /// Events the heartbeat listener thread sends back to the main loop.
    pub enum HeartbeatEvent {
        /// Child successfully connected.
        Connected,
        /// A ping byte arrived from the child.
        Ping,
        /// The connection was closed (EOF or error).
        Disconnected,
    }

    pub fn run_supervisor(
        child_bin: PathBuf,
        child_args: Vec<OsString>,
        heartbeat_timeout: Duration,
        heartbeat_disable: bool,
    ) -> anyhow::Result<()> {
        // Timestamps of the last CRASH_LIMIT crashes/hangs (rolling window).
        let mut crash_times: Vec<Instant> = Vec::new();
        let mut restart_count: u32 = 0;

        // Flag set when supervisor receives SIGINT/SIGTERM so we know
        // a forwarded-signal exit from the child is intentional.
        let shutdown_requested = Arc::new(AtomicBool::new(false));

        // Our own PID used for the socket path.
        let supervisor_pid = std::process::id();

        loop {
            // ── bind socket for this spawn ─────────────────────────────────
            let (sock_path, listener, hb_rx) = if heartbeat_disable {
                (None, None, None)
            } else {
                match setup_heartbeat_socket(supervisor_pid) {
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
                }
            };

            // ── spawn child ───────────────────────────────────────────────
            let spawn_time = Instant::now();
            let mut cmd = build_command(&child_bin, &child_args, sock_path.as_deref());
            let mut child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => {
                    anyhow::bail!("failed to spawn child {}: {e}", child_bin.display());
                }
            };

            let child_pid = Pid::from_raw(child.id() as i32);
            tracing::info!(pid = %child_pid, "child spawned");

            // ── start heartbeat listener thread ───────────────────────────
            let hb_rx = hb_rx.map(|(tx, rx)| {
                let listener = listener.expect("listener present when hb_rx present");
                std::thread::spawn(move || heartbeat_accept_loop(listener, tx));
                rx
            });

            // ── install signal forwarding ─────────────────────────────────
            let sr = Arc::clone(&shutdown_requested);
            let _signal_guard = install_signal_forwarding(child_pid, sr);

            // ── wait for heartbeat connect within grace window ────────────
            //
            // While waiting we also poll child.try_wait() so a fast-exiting
            // child (e.g. /bin/true, --help, short subcommand) is detected
            // before the 5 s grace window expires and is NOT treated as a
            // connect-timeout "crash".
            enum WatchResult {
                HangDetected,
                ChildExited(Option<std::process::ExitStatus>),
            }

            let watch_result = if let Some(ref rx) = hb_rx {
                match wait_for_connect(rx, &mut child, CONNECT_GRACE) {
                    ConnectResult::Connected => {
                        tracing::info!(pid = %child_pid, "child connected to heartbeat socket");
                        // Post-connect grace: give CEF time to initialise.
                        let post_connect_deadline =
                            Instant::now() + POST_CONNECT_GRACE + heartbeat_timeout;
                        // Now run the main ping-watch loop alongside the child.
                        if watch_heartbeat(rx, &mut child, post_connect_deadline, heartbeat_timeout)
                        {
                            WatchResult::HangDetected
                        } else {
                            WatchResult::ChildExited(child.try_wait().ok().flatten())
                        }
                    }
                    ConnectResult::TimedOut => {
                        // Child didn't connect within 5 s — treat as crash.
                        tracing::warn!(
                            pid = %child_pid,
                            "child did not connect to heartbeat socket within 5s; \
                             treating as crash"
                        );
                        // Kill the child so child.wait() below doesn't block.
                        let _ = signal::killpg(child_pid, Signal::SIGKILL);
                        let _ = child.wait();
                        WatchResult::HangDetected
                    }
                    ConnectResult::ChildExited(s) => WatchResult::ChildExited(s),
                }
            } else {
                // Heartbeat disabled or socket setup failed — just wait.
                WatchResult::ChildExited(None)
            };

            // ── reap if still running ─────────────────────────────────────
            let (hang_detected, status) = match watch_result {
                WatchResult::HangDetected => (true, None),
                WatchResult::ChildExited(s) => {
                    // Reap if we haven't already.
                    let status = if s.is_some() {
                        s
                    } else {
                        match child.wait() {
                            Ok(s) => Some(s),
                            Err(e) => {
                                tracing::warn!(error = %e, "child.wait() failed");
                                None
                            }
                        }
                    };
                    (false, status)
                }
            };

            let elapsed = spawn_time.elapsed();

            // ── clean up socket ───────────────────────────────────────────
            if let Some(ref path) = sock_path {
                let _ = std::fs::remove_file(path);
            }

            // ── decide whether to restart ─────────────────────────────────
            let is_clean = status.as_ref().and_then(|s| s.code()) == Some(0) && !hang_detected;

            if is_clean {
                tracing::info!(
                    pid = %child_pid,
                    elapsed_ms = elapsed.as_millis(),
                    "child exited cleanly (exit 0); supervisor done"
                );
                return Ok(());
            }

            // If the supervisor itself was asked to shut down, don't restart.
            if shutdown_requested.load(Ordering::SeqCst) {
                tracing::info!(
                    pid = %child_pid,
                    "child exited after supervisor shutdown signal; not restarting"
                );
                return Ok(());
            }

            // Crash / hang path.
            restart_count += 1;
            let now = Instant::now();
            crash_times.push(now);

            // Evict entries older than WINDOW_SECS.
            let window_start = now - Duration::from_secs(WINDOW_SECS);
            crash_times.retain(|t| *t >= window_start);

            if hang_detected {
                tracing::info!(
                    pid = %child_pid,
                    restart_count,
                    crashes_in_window = crash_times.len(),
                    elapsed_ms = elapsed.as_millis(),
                    "child hang detected; considering restart"
                );
            } else {
                tracing::info!(
                    pid = %child_pid,
                    exit_status = ?status,
                    restart_count,
                    crashes_in_window = crash_times.len(),
                    elapsed_ms = elapsed.as_millis(),
                    "child crashed; considering restart"
                );
            }

            if crash_times.len() >= CRASH_LIMIT {
                tracing::error!(
                    "watchdog: {CRASH_LIMIT} crashes/hangs in {WINDOW_SECS}s, \
                     refusing to restart. \
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

    /// Bind a Unix-domain socket the child will connect to.
    ///
    /// Path: `${XDG_RUNTIME_DIR}/buffr-supervisor-<pid>.sock`
    /// Fallback: `/tmp/buffr-supervisor-<pid>.sock`
    ///
    /// Unlinks any stale socket at that path before binding.
    /// Sets permissions to 0600 (owner-only).
    pub fn setup_heartbeat_socket(pid: u32) -> anyhow::Result<(PathBuf, UnixListener)> {
        let filename = format!("buffr-supervisor-{pid}.sock");
        let path = if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
            PathBuf::from(xdg).join(&filename)
        } else {
            PathBuf::from("/tmp").join(&filename)
        };

        // Remove stale socket from a prior crash.
        if path.exists() {
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
    fn heartbeat_accept_loop(listener: UnixListener, tx: std::sync::mpsc::Sender<HeartbeatEvent>) {
        use std::io::Read;

        // Block until a client connects (we set non-blocking above; poll manually).
        // The main thread uses wait_for_connect with its own timeout, so we just
        // loop with short sleeps here.
        let stream = loop {
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

    enum ConnectResult {
        /// Child successfully connected to the heartbeat socket.
        Connected,
        /// Grace window elapsed with no connection.
        TimedOut,
        /// Child exited before connecting (may be a clean exit).
        ChildExited(Option<std::process::ExitStatus>),
    }

    /// Wait up to `grace` for the child to connect.
    ///
    /// Also polls `child.try_wait()` so a fast-exiting child (clean exit,
    /// short subcommand, --help flag) is not misclassified as a hang.
    fn wait_for_connect(
        rx: &std::sync::mpsc::Receiver<HeartbeatEvent>,
        child: &mut std::process::Child,
        grace: Duration,
    ) -> ConnectResult {
        let deadline = Instant::now() + grace;
        loop {
            // Check if child already exited before the grace window ends.
            match child.try_wait() {
                Ok(Some(s)) => return ConnectResult::ChildExited(Some(s)),
                Ok(None) => {}
                Err(_) => return ConnectResult::ChildExited(None),
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return ConnectResult::TimedOut;
            }
            match rx.recv_timeout(remaining.min(Duration::from_millis(100))) {
                Ok(HeartbeatEvent::Connected) => return ConnectResult::Connected,
                Ok(_) => continue, // ping before Connected — shouldn't happen but fine
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if Instant::now() >= deadline {
                        return ConnectResult::TimedOut;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return ConnectResult::TimedOut;
                }
            }
        }
    }

    /// Watch heartbeat pings; kill child on hang. Returns true if a hang was
    /// detected (child was killed by us); false if the child exited normally.
    fn watch_heartbeat(
        rx: &std::sync::mpsc::Receiver<HeartbeatEvent>,
        child: &mut std::process::Child,
        first_deadline: Instant,
        timeout: Duration,
    ) -> bool {
        let child_pid = Pid::from_raw(child.id() as i32);
        let mut last_ping = Instant::now();
        let mut deadline = first_deadline;

        loop {
            // Check if child already exited (non-blocking).
            match child.try_wait() {
                Ok(Some(_)) => return false, // exited on its own
                Ok(None) => {}
                Err(_) => return false,
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                // Hang detected.
                tracing::error!(
                    "watchdog: ui hang detected (no heartbeat for {}s); \
                     killing child pgid={}",
                    timeout.as_secs(),
                    child_pid
                );
                let _ = signal::killpg(child_pid, Signal::SIGKILL);
                let _ = child.wait();
                return true;
            }

            match rx.recv_timeout(remaining.min(Duration::from_millis(200))) {
                Ok(HeartbeatEvent::Ping) => {
                    let now = Instant::now();
                    tracing::debug!(
                        lag_ms = now.duration_since(last_ping).as_millis(),
                        "heartbeat: ping received"
                    );
                    last_ping = now;
                    deadline = now + timeout;
                }
                Ok(HeartbeatEvent::Connected) => {
                    // Shouldn't arrive here (already connected) but reset.
                    last_ping = Instant::now();
                    deadline = last_ping + timeout;
                }
                Ok(HeartbeatEvent::Disconnected) => {
                    tracing::warn!("heartbeat: child disconnected socket");
                    // Child closed the socket — treat as crash/exit, let
                    // child.wait() handle it.
                    return false;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    // No event in this slice — loop back and check deadline.
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    // Listener thread gone — child probably exited.
                    return false;
                }
            }
        }
    }

    fn build_command(
        bin: &PathBuf,
        args: &[OsString],
        sock_path: Option<&std::path::Path>,
    ) -> std::process::Command {
        use std::os::unix::process::CommandExt;

        let mut cmd = std::process::Command::new(bin);
        cmd.args(args);

        // Pass the socket path to the child via env var (if heartbeat active).
        if let Some(path) = sock_path {
            cmd.env(SUPERVISOR_SOCK_ENV, path);
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
