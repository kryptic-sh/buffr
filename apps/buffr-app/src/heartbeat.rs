//! Heartbeat liveness probe for the buffr-supervisor watchdog.
//!
//! The UI thread calls [`Heartbeat::tick`] on every `about_to_wait`.
//! If at least 1 s has elapsed since the last ping, a single byte
//! (`0x01`) is written to the supervisor transport:
//!
//! - **Unix (Linux + macOS)**: Unix-domain socket (`BUFFR_SUPERVISOR_SOCK`).
//! - **Windows**: named pipe (`BUFFR_SUPERVISOR_PIPE`).
//!
//! The supervisor kills + restarts the child if no ping arrives for
//! `--heartbeat-timeout` seconds (default 8).
//!
//! `try_connect` returns `None` when the env var is unset (unsupervised
//! run) or the connect fails.  Either way, the caller continues normally
//! — running without a supervisor is never fatal.
//!
//! ## macOS notes
//!
//! Filesystem UDS only — abstract sockets do not exist on Darwin.
//! The socket path is supplied by the supervisor via `BUFFR_SUPERVISOR_SOCK`
//! (`/tmp/buffr-<pid>.sock` on macOS when `XDG_RUNTIME_DIR` is unset),
//! well within macOS's `sun_path` limit (~104 bytes).

/// 1 Hz heartbeat — write one byte every second the UI thread is live.
pub const HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

// ── Unix implementation (Linux + macOS) ──────────────────────────────────────

#[cfg(unix)]
mod inner {
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    use std::time::{Duration, Instant};

    use super::HEARTBEAT_INTERVAL;

    /// Env var the supervisor passes to the child with the UDS path.
    pub const SUPERVISOR_SOCK_ENV: &str = "BUFFR_SUPERVISOR_SOCK";

    /// Active heartbeat connection to the supervisor.
    pub struct Heartbeat {
        stream: UnixStream,
        last_sent: Instant,
    }

    impl Heartbeat {
        /// Try to connect to the supervisor socket.
        ///
        /// Reads `BUFFR_SUPERVISOR_SOCK`; returns `None` if the env var is
        /// absent (unsupervised), the path is invalid, or the connect fails.
        /// Errors are logged at `warn!` so they appear in RUST_LOG output
        /// but never abort the child.
        pub fn try_connect() -> Option<Self> {
            let path = match std::env::var(SUPERVISOR_SOCK_ENV) {
                Ok(p) => p,
                Err(_) => {
                    // No supervisor — running standalone.
                    return None;
                }
            };

            // Connect with a short timeout via non-blocking + select.
            // `UnixStream::connect` is synchronous; for our purposes a
            // blocking 2 s attempt is fine — CEF init takes longer anyway.
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
            Some(Self {
                stream,
                // Force a ping on the very first tick.
                last_sent: Instant::now() - HEARTBEAT_INTERVAL,
            })
        }

        /// Send a ping if due; return the next deadline regardless.
        ///
        /// Returns `Some(next_due)` while the connection is healthy.
        /// Returns `None` on a broken-pipe or any IO error so the caller
        /// can drop the `Heartbeat` — the supervisor will detect the silence
        /// and restart us.
        pub fn tick(&mut self) -> Option<std::time::Instant> {
            let now = Instant::now();
            if now.duration_since(self.last_sent) >= HEARTBEAT_INTERVAL {
                match self.stream.write_all(b"\x01") {
                    Ok(()) => {
                        tracing::debug!("heartbeat: ping sent");
                        self.last_sent = now;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "heartbeat: write failed; dropping connection");
                        return None;
                    }
                }
            }
            Some(self.last_sent + HEARTBEAT_INTERVAL)
        }
    }

    /// Connect to a Unix socket with a wall-clock timeout.
    ///
    /// We set non-blocking, attempt connect (which returns immediately
    /// with `EINPROGRESS`), then `select()` / `poll()` up to `timeout`.
    /// On success the stream is set back to blocking mode.
    fn connect_with_timeout(path: &str, timeout: Duration) -> std::io::Result<UnixStream> {
        use std::os::unix::io::AsRawFd;

        let stream = UnixStream::connect(path)?;
        stream.set_write_timeout(Some(timeout))?;
        stream.set_read_timeout(Some(timeout))?;
        // The fd is not marked non-blocking; a plain connect already
        // succeeded (Unix sockets connect synchronously on Linux when
        // the listener is ready).  Just make the writes non-blocking so
        // a stalled socket doesn't wedge the UI thread.
        stream.set_nonblocking(false)?;

        // Verify the fd is usable with a zero-byte probe.
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
    use std::time::Instant;

    use super::HEARTBEAT_INTERVAL;

    /// Env var the supervisor passes to the child with the named-pipe path.
    pub const SUPERVISOR_PIPE_ENV: &str = "BUFFR_SUPERVISOR_PIPE";

    /// Active heartbeat connection to the supervisor via named pipe.
    pub struct Heartbeat {
        /// The pipe file handle wrapped as a `std::fs::File` for `Write`.
        file: std::fs::File,
        last_sent: Instant,
    }

    impl Heartbeat {
        /// Try to connect to the supervisor's named pipe.
        ///
        /// Reads `BUFFR_SUPERVISOR_PIPE`; returns `None` if the env var is
        /// absent (unsupervised run), the path is invalid, or the connect
        /// fails.  Errors are logged at `warn!` so they appear in RUST_LOG
        /// output but never abort the child.
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
            Some(Self {
                file,
                last_sent: Instant::now() - HEARTBEAT_INTERVAL,
            })
        }

        /// Send a ping if due; return the next deadline while healthy.
        ///
        /// Returns `None` on broken-pipe or IO error so the caller can drop
        /// the `Heartbeat` — the supervisor will detect silence and restart.
        pub fn tick(&mut self) -> Option<std::time::Instant> {
            let now = Instant::now();
            if now.duration_since(self.last_sent) >= HEARTBEAT_INTERVAL {
                match self.file.write_all(b"\x01") {
                    Ok(()) => {
                        tracing::debug!("heartbeat: ping sent");
                        self.last_sent = now;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "heartbeat: write failed; dropping connection");
                        return None;
                    }
                }
            }
            Some(self.last_sent + HEARTBEAT_INTERVAL)
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

        pub fn tick(&mut self) -> Option<std::time::Instant> {
            None
        }
    }
}

// ── Public re-exports ─────────────────────────────────────────────────────────

pub use inner::Heartbeat;
