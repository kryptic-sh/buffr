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
        /// Cleared when the background thread observes a fatal write
        /// error so the next `mark_alive` does not log spurious staleness.
        thread_alive: Arc<AtomicBool>,
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

            let alive_clone = Arc::clone(&last_alive_us);
            let thread_alive_clone = Arc::clone(&thread_alive);
            thread::Builder::new()
                .name("buffr-heartbeat".into())
                .spawn(move || {
                    run_heartbeat_loop(stream, epoch, alive_clone, thread_alive_clone);
                })
                .ok()?;

            Some(Self {
                last_alive_us,
                epoch,
                thread_alive,
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

        /// Whether the background heartbeat thread is still alive
        /// (i.e. no fatal IO error has occurred).  The UI thread uses
        /// this to skip per-event work when the connection is gone.
        pub fn is_alive(&self) -> bool {
            self.thread_alive.load(Ordering::Relaxed)
        }
    }

    /// Background thread: send a ping every `HEARTBEAT_INTERVAL` while
    /// the UI thread is fresh.  Exits when the UI thread stops marking
    /// itself for `UI_LIVENESS_TIMEOUT` — the supervisor watchdog then
    /// declares a hang on its own deadline and restarts us.
    fn run_heartbeat_loop(
        mut stream: UnixStream,
        epoch: Instant,
        last_alive_us: Arc<AtomicU64>,
        thread_alive: Arc<AtomicBool>,
    ) {
        loop {
            thread::sleep(HEARTBEAT_INTERVAL);

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
            if staleness > UI_LIVENESS_TIMEOUT {
                tracing::error!(
                    staleness_ms = staleness.as_millis(),
                    "heartbeat: UI thread silent for >{}s; stopping pings so supervisor restarts us",
                    UI_LIVENESS_TIMEOUT.as_secs()
                );
                thread_alive.store(false, Ordering::Relaxed);
                return;
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
        thread_alive: Arc<AtomicBool>,
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

            let alive_clone = Arc::clone(&last_alive_us);
            let thread_alive_clone = Arc::clone(&thread_alive);
            thread::Builder::new()
                .name("buffr-heartbeat".into())
                .spawn(move || {
                    run_heartbeat_loop(file, epoch, alive_clone, thread_alive_clone);
                })
                .ok()?;

            Some(Self {
                last_alive_us,
                epoch,
                thread_alive,
            })
        }

        pub fn mark_alive(&self) {
            let us = self.epoch.elapsed().as_micros() as u64;
            self.last_alive_us.store(us, Ordering::Relaxed);
        }

        pub fn is_alive(&self) -> bool {
            self.thread_alive.load(Ordering::Relaxed)
        }
    }

    fn run_heartbeat_loop(
        mut file: std::fs::File,
        epoch: Instant,
        last_alive_us: Arc<AtomicU64>,
        thread_alive: Arc<AtomicBool>,
    ) {
        loop {
            thread::sleep(HEARTBEAT_INTERVAL);

            let last_us = last_alive_us.load(Ordering::Relaxed);
            let elapsed = epoch.elapsed();
            let staleness = if last_us == 0 {
                Duration::ZERO
            } else {
                elapsed.saturating_sub(Duration::from_micros(last_us))
            };
            if staleness > UI_LIVENESS_TIMEOUT {
                tracing::error!(
                    staleness_ms = staleness.as_millis(),
                    "heartbeat: UI thread silent for >{}s; stopping pings so supervisor restarts us",
                    UI_LIVENESS_TIMEOUT.as_secs()
                );
                thread_alive.store(false, Ordering::Relaxed);
                return;
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

        pub fn is_alive(&self) -> bool {
            false
        }
    }
}

// ── Public re-exports ─────────────────────────────────────────────────────────

pub use inner::Heartbeat;
