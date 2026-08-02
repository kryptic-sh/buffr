//! Heartbeat liveness probe for the buffr-supervisor watchdog.
//!
//! A dedicated background thread owns the supervisor socket and writes
//! one byte (`0x01`) every [`HEARTBEAT_INTERVAL`].  The UI thread does
//! NOT touch the socket — it only stores a monotonic "last alive"
//! timestamp via [`Heartbeat::mark_alive`].  The background thread
//! consults that timestamp before each write and stops pinging if the
//! UI thread has been silent for longer than [`UI_LIVENESS_TIMEOUT`],
//! at which point the supervisor's deadline expires and the child is
//! restarted.
//!
//! **The connection stays open while pings are withheld.**  Dropping it
//! would make the supervisor read EOF, which it interprets as "the child
//! is on its way out" — it would then block in `wait()` on a browser that
//! is very much still running and never restart it.  Holding the socket
//! open and simply not writing is what makes the supervisor's own
//! heartbeat deadline expire, which is the documented contract above.
//!
//! Decoupling the write from the event loop fixes two failure modes:
//!
//!   1. winit's `ControlFlow::WaitUntil` is not honoured reliably on
//!      every Wayland compositor when no frame callback arrives, so
//!      relying on `about_to_wait` / `new_events` to drive the ping
//!      lets the supervisor declare a hang on a perfectly healthy
//!      idle UI thread.
//!   2. A single slow `write_all` on the UDS socket from the UI thread
//!      previously dropped the connection handle outright (the
//!      `tick() -> Option` contract treated any IO error as fatal),
//!      after which no pings ever fired again for the lifetime of the
//!      process.
//!
//! The background thread retries on transient errors and stops only on
//! a UI-thread-liveness timeout or fatal broken-pipe error.  Supported
//! transports:
//!
//! - **Unix (Linux + macOS)**: Unix-domain socket (`BUFFR_SUPERVISOR_SOCK`).
//! - **Windows**: named pipe (`BUFFR_SUPERVISOR_PIPE`).
//!
//! `try_connect` returns `None` when the env var is unset (unsupervised
//! run) or the connect fails — running without a supervisor is never
//! fatal.

/// 1 Hz heartbeat — write one byte every second while the UI is alive.
pub const HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// If the UI thread has not called `mark_alive` for this long, the
/// background thread stops pinging so the supervisor's watchdog fires.
/// Must be shorter than the supervisor-side timeout (default 8 s).
pub const UI_LIVENESS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

// ── Unix implementation (Linux + macOS) ──────────────────────────────────────

#[cfg(unix)]
mod inner {
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::{HEARTBEAT_INTERVAL, UI_LIVENESS_TIMEOUT};

    /// Env var the supervisor passes to the child with the UDS path.
    pub const SUPERVISOR_SOCK_ENV: &str = "BUFFR_SUPERVISOR_SOCK";

    /// Public handle held by the UI thread.  Cheap to clone (`Arc`).
    /// Dropping the last handle stops the background thread.
    pub struct Heartbeat {
        last_alive_us: Arc<AtomicU64>,
        epoch: Instant,
        /// Set once, and only, on the terminal write-error path.  The
        /// background thread's other "not pinging" state — the UI
        /// thread having gone stale — is recoverable and deliberately
        /// invisible here, so the UI keeps marking itself alive and the
        /// loop's recovery path can fire.
        thread_fatal: Arc<AtomicBool>,
    }

    impl Heartbeat {
        /// Try to connect to the supervisor socket and spawn the
        /// heartbeat thread.
        ///
        /// Reads `BUFFR_SUPERVISOR_SOCK`; returns `None` when the env var
        /// is absent (unsupervised), the path is invalid, or connect
        /// fails.  Errors are logged at `warn!` but never abort the child.
        pub fn try_connect() -> Option<Self> {
            let path = match std::env::var(SUPERVISOR_SOCK_ENV) {
                Ok(p) => p,
                Err(_) => return None,
            };

            let stream = match connect_with_timeout(&path, Duration::from_secs(2)) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        path = %path,
                        "heartbeat: connect failed; running unsupervised"
                    );
                    return None;
                }
            };

            tracing::info!(path = %path, "heartbeat: connected to supervisor socket");

            let epoch = Instant::now();
            // Seed the atomic to "now" so the bg thread sends at least
            // one ping before the UI thread has a chance to mark itself.
            let last_alive_us = Arc::new(AtomicU64::new(0));
            let thread_alive = Arc::new(AtomicBool::new(true));
            let thread_fatal = Arc::new(AtomicBool::new(false));

            let alive_clone = Arc::clone(&last_alive_us);
            let thread_fatal_clone = Arc::clone(&thread_fatal);
            thread::Builder::new()
                .name("buffr-heartbeat".into())
                .spawn(move || {
                    run_heartbeat_loop(
                        stream,
                        epoch,
                        alive_clone,
                        thread_alive,
                        thread_fatal_clone,
                        HEARTBEAT_INTERVAL,
                        UI_LIVENESS_TIMEOUT,
                    );
                })
                .ok()?;

            Some(Self {
                last_alive_us,
                epoch,
                thread_fatal,
            })
        }

        /// Record a UI-thread liveness pulse.  Cheap atomic store; safe
        /// to call from any winit lifecycle hook (`new_events`,
        /// `window_event`, `about_to_wait`).  No syscalls, no allocation.
        pub fn mark_alive(&self) {
            // Microseconds since the heartbeat epoch — fits in a u64 for
            // ~584 000 years which is comfortably forever.
            let us = self.epoch.elapsed().as_micros() as u64;
            self.last_alive_us.store(us, Ordering::Relaxed);
        }

        /// Whether the background thread died on an unrecoverable write
        /// error.  Only then is dropping the handle correct: a merely
        /// stale heartbeat must keep receiving `mark_alive` so the
        /// loop's recovery path can resume pinging.
        pub fn is_fatal(&self) -> bool {
            self.thread_fatal.load(Ordering::Relaxed)
        }

        /// One UI-thread tick: stamp liveness, then report whether the
        /// caller should keep the handle.
        ///
        /// Returns `false` only on a terminal failure.  A stall is
        /// deliberately NOT terminal: the loop withholds pings but can
        /// recover, and it can only see the recovery if the UI thread
        /// keeps calling this — dropping the handle on a stall makes
        /// that recovery unreachable and turns a transient GPU block
        /// into a supervisor kill of a healthy browser.
        pub fn tick(&self) -> bool {
            self.mark_alive();
            !self.is_fatal()
        }
    }

    /// Background thread: send a ping every `interval` while the UI
    /// thread is fresh.  When the UI thread stops marking itself for
    /// `liveness_timeout` the loop **withholds pings but keeps the
    /// socket open** so the supervisor's own deadline expires and it
    /// kills + restarts the frozen child.
    ///
    /// Returning here (and thereby closing the socket) would be wrong:
    /// the supervisor reads EOF, concludes the child exited, and blocks
    /// forever in `wait()` on a process that is still running.
    ///
    /// `interval` / `liveness_timeout` are parameters rather than the
    /// module constants so the unit tests can drive the loop in
    /// milliseconds instead of seconds.
    fn run_heartbeat_loop(
        mut stream: UnixStream,
        epoch: Instant,
        last_alive_us: Arc<AtomicU64>,
        thread_alive: Arc<AtomicBool>,
        thread_fatal: Arc<AtomicBool>,
        interval: Duration,
        liveness_timeout: Duration,
    ) {
        // Set once we enter the "UI is wedged" state so the error is
        // logged a single time rather than every interval.
        let mut stalled = false;

        loop {
            thread::sleep(interval);

            // Liveness check.  `last_alive_us == 0` is the seeded value
            // — treat it as "fresh enough" so the first ping fires even
            // if the UI thread hasn't reached its first event yet.
            let last_us = last_alive_us.load(Ordering::Relaxed);
            let elapsed = epoch.elapsed();
            let staleness = if last_us == 0 {
                Duration::ZERO
            } else {
                elapsed.saturating_sub(Duration::from_micros(last_us))
            };
            if staleness > liveness_timeout {
                if !stalled {
                    tracing::error!(
                        staleness_ms = staleness.as_millis(),
                        "heartbeat: UI thread silent for >{}s; withholding pings (socket stays \
                         open) so the supervisor's deadline expires and restarts us",
                        liveness_timeout.as_secs()
                    );
                    thread_alive.store(false, Ordering::Relaxed);
                    stalled = true;
                }
                continue;
            }

            if stalled {
                // The UI thread came back before the supervisor killed
                // us — resume pinging rather than staying wedged.
                tracing::warn!("heartbeat: UI thread recovered; resuming pings");
                thread_alive.store(true, Ordering::Relaxed);
                stalled = false;
            }

            match stream.write_all(b"\x01") {
                Ok(()) => {
                    tracing::trace!("heartbeat: ping sent");
                }
                Err(e) => {
                    let kind = e.kind();
                    if kind == std::io::ErrorKind::WouldBlock
                        || kind == std::io::ErrorKind::Interrupted
                        || kind == std::io::ErrorKind::TimedOut
                    {
                        // Transient — supervisor reader is slow but
                        // the socket is still open.  Skip this tick
                        // and try again next interval.
                        tracing::debug!(error = %e, "heartbeat: transient write error; retrying");
                        continue;
                    }
                    tracing::warn!(error = %e, "heartbeat: fatal write error; thread exiting");
                    thread_fatal.store(true, Ordering::Relaxed);
                    thread_alive.store(false, Ordering::Relaxed);
                    return;
                }
            }
        }
    }

    /// Connect to a Unix socket synchronously.  Sets a generous write
    /// timeout so the background thread can recover from a stalled
    /// supervisor reader instead of blocking forever.
    fn connect_with_timeout(path: &str, timeout: Duration) -> std::io::Result<UnixStream> {
        use std::os::unix::io::AsRawFd;

        let stream = UnixStream::connect(path)?;
        stream.set_write_timeout(Some(timeout))?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_nonblocking(false)?;

        let fd = stream.as_raw_fd();
        if fd < 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "invalid fd after connect",
            ));
        }

        Ok(stream)
    }

    // ── Tests ────────────────────────────────────────────────────────────────

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::io::Read;
        use std::os::unix::net::UnixListener;

        /// Millisecond-scale stand-ins for `HEARTBEAT_INTERVAL` /
        /// `UI_LIVENESS_TIMEOUT` so the tests finish in well under a second.
        const TEST_INTERVAL: Duration = Duration::from_millis(20);
        const TEST_LIVENESS: Duration = Duration::from_millis(60);

        /// Bind a socketpair-ish UDS in a temp dir and return
        /// `(server_side, client_side)`.
        fn socket_pair() -> (UnixStream, UnixStream, tempfile::TempDir) {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("hb.sock");
            let listener = UnixListener::bind(&path).expect("bind");
            let client = UnixStream::connect(&path).expect("connect");
            let (server, _) = listener.accept().expect("accept");
            (server, client, dir)
        }

        #[test]
        fn pings_flow_while_ui_is_fresh() {
            let (mut server, client, _dir) = socket_pair();
            server
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();

            let last_alive_us = Arc::new(AtomicU64::new(0));
            let thread_alive = Arc::new(AtomicBool::new(true));
            let thread_fatal = Arc::new(AtomicBool::new(false));
            let epoch = Instant::now();
            let ta = Arc::clone(&thread_alive);
            let tf = Arc::clone(&thread_fatal);
            let la = Arc::clone(&last_alive_us);
            let h = thread::spawn(move || {
                run_heartbeat_loop(client, epoch, la, ta, tf, TEST_INTERVAL, TEST_LIVENESS);
            });

            let mut buf = [0u8; 1];
            let n = server.read(&mut buf).expect("expected a ping byte");
            assert_eq!(n, 1);
            assert_eq!(buf[0], 0x01);
            assert!(thread_alive.load(Ordering::Relaxed));
            assert!(!thread_fatal.load(Ordering::Relaxed));

            drop(server);
            let _ = h.join();
        }

        /// Block until `flag` is false, panicking after `limit`.
        fn wait_until_stale(flag: &AtomicBool, limit: Duration) {
            let deadline = Instant::now() + limit;
            while flag.load(Ordering::Relaxed) {
                assert!(
                    Instant::now() < deadline,
                    "heartbeat thread never declared the UI stale"
                );
                thread::sleep(Duration::from_millis(5));
            }
        }

        /// Read and discard everything currently buffered. Panics on EOF,
        /// which is the exact failure H12 describes.
        fn drain_pings(server: &mut UnixStream) {
            server
                .set_read_timeout(Some(Duration::from_millis(50)))
                .expect("set_read_timeout");
            let mut buf = [0u8; 256];
            loop {
                match server.read(&mut buf) {
                    Ok(0) => panic!(
                        "heartbeat thread closed the socket — the supervisor would read EOF \
                         and never restart the hung child"
                    ),
                    Ok(_) => continue,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut =>
                    {
                        return;
                    }
                    Err(e) => panic!("unexpected read error: {e}"),
                }
            }
        }

        /// H12 regression: a wedged UI must NOT close the socket.
        ///
        /// If the loop returns (dropping the stream), the supervisor reads
        /// EOF, concludes the child exited, and blocks in `wait()` forever
        /// on a still-running frozen browser. The loop must instead stop
        /// writing and hold the connection open so the supervisor's own
        /// heartbeat deadline expires and it kills + restarts the child.
        #[test]
        fn stale_ui_withholds_pings_but_keeps_socket_open() {
            let (mut server, client, _dir) = socket_pair();

            let epoch = Instant::now();
            // Mark "alive" 1 µs after the epoch and never again: staleness
            // grows past TEST_LIVENESS within a couple of intervals.
            let last_alive_us = Arc::new(AtomicU64::new(1));
            let thread_alive = Arc::new(AtomicBool::new(true));
            let thread_fatal = Arc::new(AtomicBool::new(false));
            let ta = Arc::clone(&thread_alive);
            let tf = Arc::clone(&thread_fatal);
            let la = Arc::clone(&last_alive_us);
            let h = thread::spawn(move || {
                run_heartbeat_loop(client, epoch, la, ta, tf, TEST_INTERVAL, TEST_LIVENESS);
            });

            wait_until_stale(&thread_alive, Duration::from_secs(5));
            // A stall is recoverable, so it must NOT look terminal to the
            // UI thread — that is what keeps `mark_alive` coming.
            assert!(!thread_fatal.load(Ordering::Relaxed));
            drain_pings(&mut server);

            // The decisive assertion: still no EOF and no further pings —
            // a read must time out rather than return Ok(0) or data.
            server
                .set_read_timeout(Some(TEST_LIVENESS * 4))
                .expect("set_read_timeout");
            let mut buf = [0u8; 64];
            match server.read(&mut buf) {
                Ok(0) => panic!("socket was closed by the heartbeat thread; supervisor sees EOF"),
                Ok(n) => panic!("unexpected ping burst ({n} bytes) after the UI went stale"),
                Err(e) => assert!(
                    e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut,
                    "expected a read timeout (socket held open, no pings), got {e}"
                ),
            }

            // Tear down: close the server end, then un-stale the UI so the
            // loop resumes writing, hits EPIPE, and returns. (While stalled
            // it never writes, so it would never notice the closed socket —
            // which is exactly the property under test.)
            drop(server);
            last_alive_us.store(epoch.elapsed().as_micros() as u64, Ordering::Relaxed);
            let _ = h.join();
        }

        /// A UI that recovers before the supervisor kills it resumes pinging.
        #[test]
        fn recovered_ui_resumes_pinging() {
            let (mut server, client, _dir) = socket_pair();

            let epoch = Instant::now();
            let last_alive_us = Arc::new(AtomicU64::new(1));
            let thread_alive = Arc::new(AtomicBool::new(true));
            let thread_fatal = Arc::new(AtomicBool::new(false));
            let ta = Arc::clone(&thread_alive);
            let tf = Arc::clone(&thread_fatal);
            let la = Arc::clone(&last_alive_us);
            let h = thread::spawn(move || {
                run_heartbeat_loop(client, epoch, la, ta, tf, TEST_INTERVAL, TEST_LIVENESS);
            });

            wait_until_stale(&thread_alive, Duration::from_secs(5));
            drain_pings(&mut server);

            // Start marking alive again from a background thread.
            let stop = Arc::new(AtomicBool::new(false));
            let stop_c = Arc::clone(&stop);
            let la2 = Arc::clone(&last_alive_us);
            let marker = thread::spawn(move || {
                while !stop_c.load(Ordering::Relaxed) {
                    la2.store(epoch.elapsed().as_micros() as u64, Ordering::Relaxed);
                    thread::sleep(Duration::from_millis(5));
                }
            });

            server
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("set_read_timeout");
            let mut buf = [0u8; 1];
            let n = server.read(&mut buf).expect("expected pings to resume");
            assert_eq!(n, 1);
            assert!(thread_alive.load(Ordering::Relaxed));

            stop.store(true, Ordering::Relaxed);
            let _ = marker.join();
            drop(server);
            let _ = h.join();
        }

        /// End-to-end recovery through the **production** UI-side policy.
        ///
        /// `recovered_ui_resumes_pinging` above pokes `last_alive_us`
        /// directly, so it passes even when the real UI thread would have
        /// thrown its handle away. This one drives [`Heartbeat::tick`] —
        /// exactly what `AppState::tick_heartbeat` calls — so a policy that
        /// treats a stall as terminal stops the marks, the loop never sees
        /// the recovery, and the socket read below times out.
        #[test]
        fn transient_ui_stall_recovers_through_tick_policy() {
            let (mut server, client, _dir) = socket_pair();

            let epoch = Instant::now();
            // Marked alive 1 µs after the epoch and never again: the UI is
            // "blocked in write_texture" and the loop will declare it stale.
            let last_alive_us = Arc::new(AtomicU64::new(1));
            let thread_alive = Arc::new(AtomicBool::new(true));
            let thread_fatal = Arc::new(AtomicBool::new(false));

            let hb = Heartbeat {
                last_alive_us: Arc::clone(&last_alive_us),
                epoch,
                thread_fatal: Arc::clone(&thread_fatal),
            };

            let ta = Arc::clone(&thread_alive);
            let tf = Arc::clone(&thread_fatal);
            let la = Arc::clone(&last_alive_us);
            let loop_thread = thread::spawn(move || {
                run_heartbeat_loop(client, epoch, la, ta, tf, TEST_INTERVAL, TEST_LIVENESS);
            });

            wait_until_stale(&thread_alive, Duration::from_secs(5));
            assert!(
                !thread_fatal.load(Ordering::Relaxed),
                "a stall is recoverable and must not be reported as terminal"
            );
            drain_pings(&mut server);

            // The GPU call returned: the UI thread starts ticking again,
            // running the same keep-or-drop decision as the event loop.
            let stop = Arc::new(AtomicBool::new(false));
            let stop_c = Arc::clone(&stop);
            let ui = thread::spawn(move || {
                let mut handle = Some(hb);
                while !stop_c.load(Ordering::Relaxed) {
                    if let Some(h) = handle.as_ref()
                        && !h.tick()
                    {
                        handle = None;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                handle.is_some()
            });

            // Pings must not merely restart, they must KEEP flowing. A UI
            // thread that marks itself once and then lets its handle go
            // still buys a short tail: the loop pings until staleness
            // crosses TEST_LIVENESS again, i.e. TEST_LIVENESS/TEST_INTERVAL
            // of them. Read several times that many, so only sustained
            // marking can satisfy it.
            let tail_pings = (TEST_LIVENESS.as_micros() / TEST_INTERVAL.as_micros()) as usize;
            let required = 4 * tail_pings;
            server
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("set_read_timeout");
            let mut buf = [0u8; 1];
            for i in 0..required {
                let n = server.read(&mut buf).unwrap_or_else(|e| {
                    panic!("ping {i} of {required} never arrived after the UI recovered: {e}")
                });
                assert_eq!(n, 1);
                assert_eq!(buf[0], 0x01);
            }
            assert!(
                thread_alive.load(Ordering::Relaxed),
                "the loop must clear the stall when the UI thread comes back"
            );

            stop.store(true, Ordering::Relaxed);
            let kept = ui.join().expect("ui thread");
            assert!(
                kept,
                "a recoverable stall must not make the UI thread drop its heartbeat handle"
            );

            drop(server);
            let _ = loop_thread.join();
        }

        /// The other half of the fix: a genuinely dead heartbeat must still
        /// be dropped, so keeping the handle through a stall cannot make a
        /// broken socket look healthy forever.
        #[test]
        fn fatal_write_error_is_terminal_and_drops_the_handle() {
            let (server, client, _dir) = socket_pair();

            let epoch = Instant::now();
            let last_alive_us = Arc::new(AtomicU64::new(0));
            let thread_alive = Arc::new(AtomicBool::new(true));
            let thread_fatal = Arc::new(AtomicBool::new(false));

            let hb = Heartbeat {
                last_alive_us: Arc::clone(&last_alive_us),
                epoch,
                thread_fatal: Arc::clone(&thread_fatal),
            };

            // Supervisor end gone: the loop's very first write gets EPIPE.
            drop(server);

            let ta = Arc::clone(&thread_alive);
            let tf = Arc::clone(&thread_fatal);
            let la = Arc::clone(&last_alive_us);
            let loop_thread = thread::spawn(move || {
                run_heartbeat_loop(client, epoch, la, ta, tf, TEST_INTERVAL, TEST_LIVENESS);
            });

            loop_thread.join().expect("loop must exit on a fatal write");
            assert!(
                thread_fatal.load(Ordering::Relaxed),
                "a fatal write error must be recorded as terminal"
            );
            assert!(!thread_alive.load(Ordering::Relaxed));

            // The UI-side policy must now throw the handle away.
            let mut handle = Some(hb);
            if let Some(h) = handle.as_ref()
                && !h.tick()
            {
                handle = None;
            }
            assert!(
                handle.is_none(),
                "a dead heartbeat must be dropped so the supervisor restarts us"
            );
        }
    }
}

// ── Windows implementation (named pipe) ─────────────────────────────────────

#[cfg(windows)]
mod inner {
    use std::io::Write;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::{HEARTBEAT_INTERVAL, UI_LIVENESS_TIMEOUT};

    pub const SUPERVISOR_PIPE_ENV: &str = "BUFFR_SUPERVISOR_PIPE";

    pub struct Heartbeat {
        last_alive_us: Arc<AtomicU64>,
        epoch: Instant,
        /// See the Unix twin: terminal write failures only.  A stall is
        /// recoverable and deliberately invisible here.
        thread_fatal: Arc<AtomicBool>,
    }

    impl Heartbeat {
        pub fn try_connect() -> Option<Self> {
            use std::os::windows::io::FromRawHandle;
            use windows_sys::Win32::Foundation::{GENERIC_WRITE, INVALID_HANDLE_VALUE};
            use windows_sys::Win32::Storage::FileSystem::{
                CreateFileW, FILE_SHARE_NONE, OPEN_EXISTING,
            };

            let path = match std::env::var(SUPERVISOR_PIPE_ENV) {
                Ok(p) => p,
                Err(_) => return None,
            };

            let path_wide: Vec<u16> = path.encode_utf16().chain([0]).collect();

            // SAFETY: path_wide is NUL-terminated; constants are correct for
            // opening an existing named pipe for writing only.
            let handle = unsafe {
                CreateFileW(
                    path_wide.as_ptr(),
                    GENERIC_WRITE,
                    FILE_SHARE_NONE,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    0,
                    std::ptr::null_mut(),
                )
            };

            if handle == INVALID_HANDLE_VALUE || handle.is_null() {
                tracing::warn!(
                    path = %path,
                    "heartbeat: CreateFileW failed ({}); running unsupervised",
                    std::io::Error::last_os_error()
                );
                return None;
            }

            // SAFETY: handle is a valid, owned Win32 file handle.
            let file = unsafe { std::fs::File::from_raw_handle(handle as *mut _) };
            tracing::info!(path = %path, "heartbeat: connected to supervisor named pipe");

            let epoch = Instant::now();
            let last_alive_us = Arc::new(AtomicU64::new(0));
            let thread_alive = Arc::new(AtomicBool::new(true));
            let thread_fatal = Arc::new(AtomicBool::new(false));

            let alive_clone = Arc::clone(&last_alive_us);
            let thread_fatal_clone = Arc::clone(&thread_fatal);
            thread::Builder::new()
                .name("buffr-heartbeat".into())
                .spawn(move || {
                    run_heartbeat_loop(
                        file,
                        epoch,
                        alive_clone,
                        thread_alive,
                        thread_fatal_clone,
                        HEARTBEAT_INTERVAL,
                        UI_LIVENESS_TIMEOUT,
                    );
                })
                .ok()?;

            Some(Self {
                last_alive_us,
                epoch,
                thread_fatal,
            })
        }

        pub fn mark_alive(&self) {
            let us = self.epoch.elapsed().as_micros() as u64;
            self.last_alive_us.store(us, Ordering::Relaxed);
        }

        /// See the Unix twin.
        pub fn is_fatal(&self) -> bool {
            self.thread_fatal.load(Ordering::Relaxed)
        }

        /// See the Unix twin: `false` only on a terminal failure, never
        /// on a recoverable stall.
        pub fn tick(&self) -> bool {
            self.mark_alive();
            !self.is_fatal()
        }
    }

    /// See the Unix twin for why a wedged UI must keep the pipe **open**
    /// while withholding pings instead of closing it.
    fn run_heartbeat_loop(
        mut file: std::fs::File,
        epoch: Instant,
        last_alive_us: Arc<AtomicU64>,
        thread_alive: Arc<AtomicBool>,
        thread_fatal: Arc<AtomicBool>,
        interval: Duration,
        liveness_timeout: Duration,
    ) {
        let mut stalled = false;

        loop {
            thread::sleep(interval);

            let last_us = last_alive_us.load(Ordering::Relaxed);
            let elapsed = epoch.elapsed();
            let staleness = if last_us == 0 {
                Duration::ZERO
            } else {
                elapsed.saturating_sub(Duration::from_micros(last_us))
            };
            if staleness > liveness_timeout {
                if !stalled {
                    tracing::error!(
                        staleness_ms = staleness.as_millis(),
                        "heartbeat: UI thread silent for >{}s; withholding pings (pipe stays \
                         open) so the supervisor's deadline expires and restarts us",
                        liveness_timeout.as_secs()
                    );
                    thread_alive.store(false, Ordering::Relaxed);
                    stalled = true;
                }
                continue;
            }

            if stalled {
                tracing::warn!("heartbeat: UI thread recovered; resuming pings");
                thread_alive.store(true, Ordering::Relaxed);
                stalled = false;
            }

            match file.write_all(b"\x01") {
                Ok(()) => {
                    tracing::trace!("heartbeat: ping sent");
                }
                Err(e) => {
                    let kind = e.kind();
                    if kind == std::io::ErrorKind::WouldBlock
                        || kind == std::io::ErrorKind::Interrupted
                        || kind == std::io::ErrorKind::TimedOut
                    {
                        tracing::debug!(error = %e, "heartbeat: transient write error; retrying");
                        continue;
                    }
                    tracing::warn!(error = %e, "heartbeat: fatal write error; thread exiting");
                    thread_fatal.store(true, Ordering::Relaxed);
                    thread_alive.store(false, Ordering::Relaxed);
                    return;
                }
            }
        }
    }
}

// ── Non-Unix/non-Windows stub ─────────────────────────────────────────────────

#[cfg(not(any(unix, windows)))]
mod inner {
    /// No-op stub on platforms with no supervisor support.
    pub struct Heartbeat {
        _private: (),
    }

    impl Heartbeat {
        pub fn try_connect() -> Option<Self> {
            None
        }

        pub fn mark_alive(&self) {}

        pub fn is_fatal(&self) -> bool {
            false
        }

        /// No supervisor here, so nothing is ever terminal — and
        /// `try_connect` never hands out a handle anyway.
        pub fn tick(&self) -> bool {
            true
        }
    }
}

// ── Public re-exports ─────────────────────────────────────────────────────────

pub use inner::Heartbeat;
