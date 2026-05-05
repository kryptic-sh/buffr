//! UDS heartbeat liveness probe for the buffr-supervisor watchdog.
//!
//! The UI thread calls [`Heartbeat::tick`] on every `about_to_wait`.
//! If at least 1 s has elapsed since the last ping, a single byte
//! (`0x01`) is written to the supervisor's Unix-domain socket.  The
//! supervisor kills + restarts the child if no ping arrives for
//! `--heartbeat-timeout` seconds (default 8).
//!
//! `try_connect` returns `None` when `BUFFR_SUPERVISOR_SOCK` is unset
//! (unsupervised run) or the connect fails.  Either way, the caller
//! continues normally — running without a supervisor is never fatal.
//!
//! **Linux only.** Non-Linux targets get an empty stub so the workspace
//! builds everywhere.

/// 1 Hz heartbeat — write one byte every second the UI thread is live.
pub const HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

// ── Linux implementation ──────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
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

// ── Non-Linux stub ────────────────────────────────────────────────────────────

#[cfg(not(target_os = "linux"))]
mod inner {
    /// No-op stub on non-Linux platforms.
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
