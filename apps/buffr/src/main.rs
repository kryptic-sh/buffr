//! buffr — crash-restart + hang watchdog supervisor for the buffr browser.
//!
//! **Linux and macOS** use Unix-domain-socket heartbeats + `setsid`/`killpg`.
//! **Windows** (Round 5) uses Job Objects with `KILL_ON_JOB_CLOSE` and a
//! named-pipe heartbeat.  On all other platforms the supervisor prints a
//! notice and execs the child binary directly without a watchdog loop.
//! This keeps `cargo build --workspace` green on every platform's CI.
//!
//! ## macOS socket path
//!
//! Filesystem UDS only (abstract sockets do not exist on Darwin).
//! `XDG_RUNTIME_DIR` is typically unset on macOS so the socket lands in
//! `/tmp/buffr-<pid>.sock` — well within macOS's stricter `sun_path` limit
//! (~104 bytes; `/tmp/buffr-99999.sock` is 23 bytes).
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
        if p.is_file() {
            return Ok(p);
        }
        // Still return it — let exec fail with a clear OS error.
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
    /// Sized to absorb cold-disk first-run setup (CEF lib load, SQLite db
    /// init, etc.). 5 s was too tight on slower disks. Integration tests
    /// override via `BUFFR_CONNECT_GRACE_MS` to keep their runtime bounded.
    const DEFAULT_CONNECT_GRACE: Duration = Duration::from_secs(20);
    /// Additional grace after the child connects before enforcing heartbeat.
    const POST_CONNECT_GRACE: Duration = Duration::from_millis(1500);

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

            // ── clean-shutdown flag path ──────────────────────────────────
            // Sibling of the heartbeat socket; supervisor owns it. Child
            // touches the file when it's about to do an intentional
            // exit; supervisor checks it after the child exits and
            // suppresses restart when present, regardless of the
            // actual exit status. Each spawn gets a fresh path so a
            // stale flag from a prior child can't suppress a real
            // crash restart.
            let clean_flag_path = sock_path.as_ref().map(|p| {
                let mut q = p.clone();
                q.set_extension("clean");
                // Remove any stale flag from a prior spawn.
                let _ = std::fs::remove_file(&q);
                q
            });

            // ── spawn child ───────────────────────────────────────────────
            let spawn_time = Instant::now();
            let mut cmd = build_command(
                &child_bin,
                &child_args,
                sock_path.as_deref(),
                clean_flag_path.as_deref(),
            );
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
                match wait_for_connect(rx, &mut child, connect_grace()) {
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
            // "Clean" means EITHER exit code 0 with no hang, OR the
            // child touched the clean-shutdown flag before exiting
            // (covers segfaults during CEF / wgpu teardown after the
            // user explicitly closed the window — see the
            // `SUPERVISOR_CLEAN_FLAG_ENV` doc).
            let exit_zero = status.as_ref().and_then(|s| s.code()) == Some(0) && !hang_detected;
            let flag_present = clean_flag_path
                .as_ref()
                .is_some_and(|p| p.exists());
            // Remove the flag eagerly — whether or not we restart, the
            // next spawn re-creates its own.
            if let Some(ref p) = clean_flag_path {
                let _ = std::fs::remove_file(p);
            }
            let is_clean = exit_zero || flag_present;

            if is_clean {
                tracing::info!(
                    pid = %child_pid,
                    elapsed_ms = elapsed.as_millis(),
                    exit_zero,
                    flag_present,
                    "child exited cleanly; supervisor done"
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

    /// Bind a Unix-domain socket the child will connect to.
    ///
    /// Path: `${XDG_RUNTIME_DIR}/buffr-<pid>.sock`
    /// Fallback: `/tmp/buffr-<pid>.sock`
    ///
    /// Unlinks any stale socket at that path before binding.
    /// Sets permissions to 0600 (owner-only).
    pub fn setup_heartbeat_socket(pid: u32) -> anyhow::Result<(PathBuf, UnixListener)> {
        let filename = format!("buffr-{pid}.sock");
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
        clean_flag_path: Option<&std::path::Path>,
    ) -> std::process::Command {
        use std::os::unix::process::CommandExt;

        let mut cmd = std::process::Command::new(bin);
        cmd.args(args);

        // Pass the socket path to the child via env var (if heartbeat active).
        if let Some(path) = sock_path {
            cmd.env(SUPERVISOR_SOCK_ENV, path);
        }

        // Pass the clean-shutdown flag path so the child can signal
        // intentional close. Supervisor reads it on exit; presence
        // overrides exit-status crash detection.
        if let Some(path) = clean_flag_path {
            cmd.env(SUPERVISOR_CLEAN_FLAG_ENV, path);
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

// ── Windows supervisor (Job Objects + named-pipe heartbeat) ─────────────────
#[cfg(windows)]
mod windows {
    use std::ffi::OsString;
    use std::path::PathBuf;
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
        CREATE_BREAKAWAY_FROM_JOB, CREATE_SUSPENDED, CreateProcessW, GetExitCodeProcess, INFINITE,
        PROCESS_INFORMATION, ResumeThread, STARTUPINFOW, TerminateProcess, WaitForSingleObject,
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

    /// Events the named-pipe reader thread sends back to the main loop.
    pub enum HeartbeatEvent {
        Connected,
        Ping,
        Disconnected,
    }

    pub fn run_supervisor(
        child_bin: PathBuf,
        child_args: Vec<OsString>,
        heartbeat_timeout: Duration,
        heartbeat_disable: bool,
    ) -> anyhow::Result<()> {
        let mut crash_times: Vec<Instant> = Vec::new();
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

            // ── spawn child (suspended) + assign to job ───────────────────────
            let spawn_time = Instant::now();
            let proc_info =
                match spawn_child_suspended(&child_bin, &child_args, pipe_path_str.as_deref()) {
                    Ok(pi) => pi,
                    Err(e) => {
                        anyhow::bail!("failed to spawn child {}: {e}", child_bin.display());
                    }
                };

            let child_pid = proc_info.dwProcessId;
            tracing::info!(pid = child_pid, "child spawned (suspended)");

            // ── assign-before-resume (critical ordering) ──────────────────────
            // SAFETY: job and process handles are valid. Assign before
            // ResumeThread so any descendants the child spawns after resume
            // also land in the job automatically.
            let assign_ok = unsafe { AssignProcessToJobObject(job.raw(), proc_info.hProcess) != 0 };
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
            enum WatchResult {
                HangDetected,
                ChildExited(Option<u32>),
            }

            let watch_result = if let Some(ref rx) = hb_rx {
                match wait_for_connect(rx, proc_info.hProcess, connect_grace()) {
                    ConnectResult::Connected => {
                        tracing::info!(pid = child_pid, "child connected to heartbeat pipe");
                        let post_connect_deadline =
                            Instant::now() + POST_CONNECT_GRACE + heartbeat_timeout;
                        if watch_heartbeat(
                            rx,
                            proc_info.hProcess,
                            post_connect_deadline,
                            heartbeat_timeout,
                        ) {
                            WatchResult::HangDetected
                        } else {
                            WatchResult::ChildExited(get_exit_code(proc_info.hProcess))
                        }
                    }
                    ConnectResult::TimedOut => {
                        tracing::warn!(
                            pid = child_pid,
                            "child did not connect to heartbeat pipe within 5s; treating as crash"
                        );
                        kill_process(proc_info.hProcess);
                        WatchResult::HangDetected
                    }
                    ConnectResult::ChildExited(code) => WatchResult::ChildExited(code),
                }
            } else {
                // Heartbeat disabled — block until child exits.
                // SAFETY: process handle is valid; INFINITE is a safe sentinel.
                unsafe { WaitForSingleObject(proc_info.hProcess, INFINITE) };
                WatchResult::ChildExited(get_exit_code(proc_info.hProcess))
            };

            // SAFETY: process handle is valid and owned.
            unsafe { CloseHandle(proc_info.hProcess) };

            let elapsed = spawn_time.elapsed();

            // ── decide whether to restart ─────────────────────────────────────
            let is_clean = matches!(&watch_result, WatchResult::ChildExited(Some(0)));

            if is_clean {
                tracing::info!(
                    pid = child_pid,
                    elapsed_ms = elapsed.as_millis(),
                    "child exited cleanly (exit 0); supervisor done"
                );
                return Ok(());
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
            let now = Instant::now();
            crash_times.push(now);
            let window_start = now - Duration::from_secs(WINDOW_SECS);
            crash_times.retain(|t| *t >= window_start);

            let hang_detected = matches!(watch_result, WatchResult::HangDetected);
            if hang_detected {
                tracing::info!(
                    pid = child_pid,
                    restart_count,
                    crashes_in_window = crash_times.len(),
                    elapsed_ms = elapsed.as_millis(),
                    "child hang detected; considering restart"
                );
            } else {
                tracing::info!(
                    pid = child_pid,
                    restart_count,
                    crashes_in_window = crash_times.len(),
                    elapsed_ms = elapsed.as_millis(),
                    "child crashed; considering restart"
                );
            }

            if crash_times.len() >= CRASH_LIMIT {
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

    fn spawn_child_suspended(
        bin: &PathBuf,
        args: &[OsString],
        pipe_path: Option<&str>,
    ) -> anyhow::Result<PROCESS_INFORMATION> {
        // Build a Windows command line: `"bin" arg1 arg2 ...`
        let mut cmdline = format!("\"{}\"", bin.display());
        for a in args {
            cmdline.push(' ');
            cmdline.push_str(&a.to_string_lossy());
        }
        let mut cmdline_wide: Vec<u16> = cmdline.encode_utf16().chain([0]).collect();

        // Pass the pipe path to the child via env var.
        // We set it on the process env so it applies only to the child.
        if let Some(path) = pipe_path {
            // SAFETY: setting env var in a single-threaded context before spawn.
            unsafe { std::env::set_var(SUPERVISOR_PIPE_ENV, path) };
        }

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
        let flags = CREATE_SUSPENDED | CREATE_BREAKAWAY_FROM_JOB;

        // SAFETY: cmdline_wide is NUL-terminated; si/pi point to valid zeroed
        // structs; NULL for application name uses the cmdline parsing path.
        let ok = unsafe {
            CreateProcessW(
                std::ptr::null(),
                cmdline_wide.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0, // bInheritHandles = FALSE
                flags,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::addr_of!(si),
                std::ptr::addr_of_mut!(pi),
            )
        };

        // Clear the env var so it doesn't leak into subsequent spawns in case
        // of restarts (each restart re-sets it with the current pipe path).
        if pipe_path.is_some() {
            // SAFETY: single-threaded context here.
            unsafe { std::env::remove_var(SUPERVISOR_PIPE_ENV) };
        }

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

    // ── Process helpers ───────────────────────────────────────────────────────

    fn get_exit_code(handle: HANDLE) -> Option<u32> {
        let mut code: u32 = 0;
        // SAFETY: handle is valid; code is initialised.
        let ok = unsafe { GetExitCodeProcess(handle, std::ptr::addr_of_mut!(code)) };
        if ok != 0 { Some(code) } else { None }
    }

    fn kill_process(handle: HANDLE) {
        // SAFETY: handle is valid; exit code 1 signals abnormal termination.
        unsafe { TerminateProcess(handle, 1) };
    }

    fn process_exited(handle: HANDLE) -> Option<u32> {
        // SAFETY: handle is valid; 0 timeout → non-blocking poll.
        let r = unsafe { WaitForSingleObject(handle, 0) };
        if r == WAIT_OBJECT_0 {
            get_exit_code(handle)
        } else {
            None
        }
    }

    // ── Connect + heartbeat watch ─────────────────────────────────────────────

    enum ConnectResult {
        Connected,
        TimedOut,
        ChildExited(Option<u32>),
    }

    fn wait_for_connect(
        rx: &std::sync::mpsc::Receiver<HeartbeatEvent>,
        proc_handle: HANDLE,
        grace: Duration,
    ) -> ConnectResult {
        let deadline = Instant::now() + grace;
        loop {
            if let Some(code) = process_exited(proc_handle) {
                return ConnectResult::ChildExited(Some(code));
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return ConnectResult::TimedOut;
            }
            match rx.recv_timeout(remaining.min(Duration::from_millis(100))) {
                Ok(HeartbeatEvent::Connected) => return ConnectResult::Connected,
                Ok(_) => continue,
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

    fn watch_heartbeat(
        rx: &std::sync::mpsc::Receiver<HeartbeatEvent>,
        proc_handle: HANDLE,
        first_deadline: Instant,
        timeout: Duration,
    ) -> bool {
        let mut last_ping = Instant::now();
        let mut deadline = first_deadline;

        loop {
            if let Some(_code) = process_exited(proc_handle) {
                return false; // exited on its own
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                tracing::error!(
                    "watchdog: ui hang detected (no heartbeat for {}s); killing child",
                    timeout.as_secs(),
                );
                kill_process(proc_handle);
                // SAFETY: INFINITE wait; we just killed the process so it will exit.
                unsafe { WaitForSingleObject(proc_handle, INFINITE) };
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
                    last_ping = Instant::now();
                    deadline = last_ping + timeout;
                }
                Ok(HeartbeatEvent::Disconnected) => {
                    tracing::warn!("heartbeat: child disconnected pipe");
                    return false;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return false;
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
