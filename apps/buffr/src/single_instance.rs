//! Single-instance / singleton locking for buffr.
//!
//! When a second `buffr` invocation fires while a first is already running on
//! the same profile, the new process forwards its URL list over a local socket
//! / named pipe and then exits 0. The running instance opens the URLs as new
//! background tabs and brings the window to the front.
//!
//! `--private` mode is exempt: the caller skips `try_acquire` entirely so each
//! private invocation always starts its own isolated process.
//!
//! ## Socket path
//!
//! - Linux: `$XDG_RUNTIME_DIR/buffr/buffr-<profile_id>.sock`, falling back to
//!   `$TMPDIR/buffr-<uid>-<profile_id>.sock`.
//! - macOS: `$TMPDIR/buffr-<uid>-<profile_id>.sock`.
//! - Windows: named pipe `\\.\pipe\buffr-<profile_id>`.
//!
//! `profile_id` is the first 8 bytes of `sha256(cache_path)` expressed as 16
//! lower-case hex digits. See [`profile_id_from`].

use std::{
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use interprocess::local_socket::{
    GenericFilePath, ListenerOptions, Stream, ToFsName,
    traits::{ListenerExt, Stream as _},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{debug, warn};

/// Payload sent from a forwarding (secondary) invocation to the singleton.
#[derive(Serialize, Deserialize, Debug)]
pub struct ForwardPayload {
    pub urls: Vec<String>,
}

/// Outcome of [`try_acquire`].
pub enum AcquireResult {
    /// This process is the singleton. Stash the handle for the accept thread.
    Owner(SingletonHandle),
    /// Successfully forwarded the request to the existing singleton. Caller exits 0.
    Forwarded,
}

/// Held by the singleton process for its lifetime.
///
/// `Drop` unlinks the socket file on Unix. On Windows the named pipe is torn
/// down automatically when the `Listener` is dropped.
pub struct SingletonHandle {
    pub(crate) listener: interprocess::local_socket::Listener,
    /// Path to unlink at drop (Unix only; empty on Windows). Read by
    /// `Drop` under `#[cfg(unix)]`; on Windows the field exists to
    /// keep a uniform constructor shape but is never read.
    #[cfg_attr(windows, allow(dead_code))]
    pub(crate) socket_path: PathBuf,
}

impl Drop for SingletonHandle {
    fn drop(&mut self) {
        #[cfg(unix)]
        if !self.socket_path.as_os_str().is_empty() {
            if let Err(e) = std::fs::remove_file(&self.socket_path) {
                debug!(path = %self.socket_path.display(), error = %e, "single_instance: unlink socket on drop failed (ignored)");
            } else {
                debug!(path = %self.socket_path.display(), "single_instance: socket unlinked on drop");
            }
        }
    }
}

/// Derive a 16-hex-character profile identifier from the cache directory path.
///
/// ```text
/// sha256(cache_path_bytes)[0..8] → lower-hex
/// ```
pub fn profile_id_from(cache_path: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(cache_path.as_bytes());
    let digest = hasher.finalize();
    hex::encode(&digest[..8])
}

/// Compute the socket path / pipe name for the given profile.
fn socket_path_for(profile_id: &str) -> PathBuf {
    #[cfg(windows)]
    {
        // Named pipe path: \\.\pipe\buffr-<profile_id>
        PathBuf::from(format!(r"\\.\pipe\buffr-{profile_id}"))
    }
    #[cfg(target_os = "linux")]
    {
        let uid = unsafe { libc::getuid() };
        if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
            let p = PathBuf::from(dir).join("buffr");
            if std::fs::create_dir_all(&p).is_ok() {
                return p.join(format!("buffr-{profile_id}.sock"));
            }
        }
        std::env::temp_dir().join(format!("buffr-{uid}-{profile_id}.sock"))
    }
    #[cfg(target_os = "macos")]
    {
        let uid = unsafe { libc::getuid() };
        std::env::temp_dir().join(format!("buffr-{uid}-{profile_id}.sock"))
    }
    // Fallback for other Unix-like systems.
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        let uid = unsafe { libc::getuid() };
        std::env::temp_dir().join(format!("buffr-{uid}-{profile_id}.sock"))
    }
}

/// Build a [`interprocess::local_socket::Name`] from a socket path.
fn make_name(path: &Path) -> Result<interprocess::local_socket::Name<'static>> {
    path.to_path_buf()
        .to_fs_name::<GenericFilePath>()
        .context("building local socket name from path")
}

/// Try to connect as a client and forward the URLs.
///
/// Returns `Ok(true)` when forwarding succeeded, `Ok(false)` when no server is
/// listening (stale or absent socket).
fn try_forward(path: &Path, urls: &[String]) -> Result<bool> {
    let name = make_name(path)?;
    let stream = match Stream::connect(name) {
        Ok(s) => s,
        Err(e) => {
            use std::io::ErrorKind::*;
            match e.kind() {
                NotFound | ConnectionRefused => {
                    debug!(error = %e, "single_instance: no server listening (stale/absent)");
                    return Ok(false);
                }
                _ => {
                    debug!(error = %e, "single_instance: connect error treated as no-server");
                    return Ok(false);
                }
            }
        }
    };
    // Set a 2-second timeout so we never hang if the server is wedged.
    stream
        .set_recv_timeout(Some(Duration::from_secs(2)))
        .context("setting recv timeout on forwarding stream")?;
    stream
        .set_send_timeout(Some(Duration::from_secs(2)))
        .context("setting send timeout on forwarding stream")?;

    let payload = ForwardPayload {
        urls: urls.to_vec(),
    };
    let line = serde_json::to_string(&payload).context("serializing ForwardPayload")?;

    // Write the JSON line.
    (&stream)
        .write_all(line.as_bytes())
        .context("writing ForwardPayload to server")?;
    (&stream)
        .write_all(b"\n")
        .context("writing newline to server")?;

    // Read ack (any non-empty line; server sends "OK\n").
    let mut ack = String::new();
    BufReader::new(&stream)
        .read_line(&mut ack)
        .context("reading ack from server")?;
    debug!(ack = %ack.trim(), "single_instance: forwarding ack received");
    Ok(true)
}

/// Attempt to bind as the singleton listener.
///
/// On Unix, removes a stale socket file before trying to bind.
fn try_bind(path: &Path) -> Result<interprocess::local_socket::Listener> {
    // On Unix, remove stale socket before binding so we don't get EADDRINUSE.
    #[cfg(unix)]
    {
        if path.exists() {
            if let Err(e) = std::fs::remove_file(path) {
                warn!(path = %path.display(), error = %e, "single_instance: could not remove stale socket (will try bind anyway)");
            } else {
                debug!(path = %path.display(), "single_instance: removed stale socket");
            }
        }
    }

    let name = make_name(path)?;
    ListenerOptions::new()
        .name(name)
        .create_sync()
        .context("binding singleton listener")
}

/// Acquire the singleton lock.
///
/// 1. Try to **connect** as a client and forward `urls`.
///    - Success → `AcquireResult::Forwarded`.
///    - ENOENT / ECONNREFUSED → stale socket. Fall through to bind.
/// 2. Remove stale socket (Unix) then **bind** as listener.
///    - Success → `AcquireResult::Owner`.
///    - EADDRINUSE (race: another process beat us) → retry connect once.
///      If that succeeds → `AcquireResult::Forwarded`; else propagate error.
pub fn try_acquire(profile_id: &str, urls: &[String]) -> Result<AcquireResult> {
    let path = socket_path_for(profile_id);
    debug!(path = %path.display(), "single_instance: acquiring singleton lock");

    // Step 1: try to forward to an existing instance.
    if try_forward(&path, urls)? {
        return Ok(AcquireResult::Forwarded);
    }

    // Step 2: no existing server — try to bind.
    match try_bind(&path) {
        Ok(listener) => {
            debug!(path = %path.display(), "single_instance: we are the singleton");
            Ok(AcquireResult::Owner(SingletonHandle {
                listener,
                socket_path: path,
            }))
        }
        Err(bind_err) => {
            // Race: someone else bound between our failed connect and our bind.
            // Retry connect once.
            debug!(error = %bind_err, "single_instance: bind failed; retrying connect");
            if try_forward(&path, urls)? {
                return Ok(AcquireResult::Forwarded);
            }
            // Both failed — propagate the original bind error.
            Err(bind_err)
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::profile_id_from;

    // ---- Group 2: profile_id sha256 derivation ------------------------------
    //
    // `profile_id_from` hashes the cache path with SHA-256 and takes the first
    // 8 bytes expressed as 16 lower-case hex digits. These tests pin the exact
    // output format, stability, and correctness on unusual inputs so regressions
    // are caught before they break the socket-path scheme.

    #[test]
    fn profile_id_is_deterministic() {
        // Same input twice must yield identical output — hash must be stable.
        let a = profile_id_from("/home/user/.cache/buffr");
        let b = profile_id_from("/home/user/.cache/buffr");
        assert_eq!(a, b);
    }

    #[test]
    fn profile_id_is_16_hex_chars() {
        // Exactly 16 lower-case hex digits (8 bytes * 2 chars/byte).
        let id = profile_id_from("/home/user/.cache/buffr");
        assert_eq!(id.len(), 16, "expected 16 chars, got {}: {id}", id.len());
        assert!(
            id.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')),
            "non-lowercase-hex char in id: {id}"
        );
    }

    #[test]
    fn profile_id_differs_for_different_paths() {
        // Basic collision smoke test: 3 distinct paths → 3 distinct ids.
        let ids = [
            profile_id_from("/home/alice/.cache/buffr"),
            profile_id_from("/home/bob/.cache/buffr"),
            profile_id_from("/tmp/buffr-test-profile"),
        ];
        // All three must be distinct.
        assert_ne!(ids[0], ids[1], "alice == bob (collision)");
        assert_ne!(ids[0], ids[2], "alice == tmp (collision)");
        assert_ne!(ids[1], ids[2], "bob == tmp (collision)");
    }

    #[test]
    fn profile_id_handles_unicode_paths() {
        // Non-ASCII path bytes must not panic — SHA-256 works on raw bytes.
        let a = profile_id_from("/tmp/缓存");
        let b = profile_id_from("/tmp/café");
        assert_eq!(a.len(), 16);
        assert_eq!(b.len(), 16);
        assert_ne!(a, b, "distinct unicode paths must not collide");
    }

    #[test]
    fn profile_id_handles_long_paths() {
        // PATH_MAX on Linux is 4096. Construct a path near that length.
        let long: String = std::iter::repeat('a').take(4090).collect();
        let path = format!("/tmp/{long}");
        let id = profile_id_from(&path);
        assert_eq!(id.len(), 16);
        assert!(
            id.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')),
            "non-hex char in long-path id: {id}"
        );
    }
}

/// Spawn a daemon thread that accepts connections from secondary invocations,
/// deserializes their [`ForwardPayload`] JSON, and forwards via `proxy`.
///
/// The `handle` is moved into the thread so the `Listener` stays alive for the
/// process lifetime. Connection errors are logged at WARN and the loop
/// continues (no crash on bad clients).
pub fn spawn_accept_thread(
    handle: SingletonHandle,
    proxy: winit::event_loop::EventLoopProxy<crate::BuffrUserEvent>,
) {
    std::thread::Builder::new()
        .name("buffr-ipc-accept".into())
        .spawn(move || {
            debug!("single_instance: accept thread started");
            // `listener.incoming()` is an infinite blocking iterator.
            // Each `next()` calls `accept()` once and blocks until a client arrives.
            for conn in handle.listener.incoming() {
                let mut stream = match conn {
                    Ok(s) => s,
                    Err(e) => {
                        warn!(error = %e, "single_instance: accept error (continuing)");
                        continue;
                    }
                };
                // Read one newline-terminated JSON line.
                let mut line = String::new();
                match BufReader::new(&stream).read_line(&mut line) {
                    Ok(0) => {
                        warn!("single_instance: client closed before sending payload");
                        continue;
                    }
                    Err(e) => {
                        warn!(error = %e, "single_instance: read error from client (continuing)");
                        continue;
                    }
                    Ok(_) => {}
                }
                let payload: ForwardPayload = match serde_json::from_str(line.trim()) {
                    Ok(p) => p,
                    Err(e) => {
                        warn!(error = %e, raw = %line.trim(), "single_instance: bad JSON payload (continuing)");
                        // Still ack so the client doesn't hang.
                        let _ = stream.write_all(b"ERR\n");
                        continue;
                    }
                };
                debug!(
                    count = payload.urls.len(),
                    "single_instance: received forwarded URLs"
                );
                // Send event to the winit loop.
                if let Err(e) =
                    proxy.send_event(crate::BuffrUserEvent::OpenUrls(payload.urls))
                {
                    warn!(error = %e, "single_instance: proxy.send_event failed (loop closed?)");
                }
                // Ack.
                let _ = stream.write_all(b"OK\n");
            }
            debug!("single_instance: accept thread exiting");
        })
        .expect("single_instance: failed to spawn accept thread");
}
