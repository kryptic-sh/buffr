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
//! Everything lives inside a **private per-uid directory** so no other local
//! user can reach — or pre-create — the socket:
//!
//! - Linux: `$XDG_RUNTIME_DIR/buffr/`, falling back to `$TMPDIR/buffr-<uid>/`
//!   created `0700` and verified (owner + mode + not a symlink) afterwards.
//! - macOS: `$TMPDIR/buffr-<uid>/`, same treatment (`XDG_RUNTIME_DIR` is
//!   normally unset there).
//! - The socket itself is `buffr-<profile_id>.sock` inside that directory,
//!   mode `0600`.
//! - Windows: named pipe `\\.\pipe\buffr-<profile_id>` (no directory
//!   involved; see [`socket_path_for`] for the caveat).
//!
//! `profile_id` is the first 8 bytes of `sha256(cache_path)` expressed as 16
//! lower-case hex digits. See [`profile_id_from`].
//!
//! ## Ownership
//!
//! On Unix the profile is owned via an `flock`ed lock file held for the
//! process lifetime, not by "whoever wrote the socket file last". The socket
//! is bound to a temporary name and `rename`d into place, so a concurrent
//! launcher never unlinks a socket another process is actively listening on.
//! Accepted connections are checked with `SO_PEERCRED` / `getpeereid`: only a
//! peer running as our own uid can hand us URLs.

use std::{
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use interprocess::local_socket::{
    GenericFilePath, ListenerOptions, Stream, ToFsName,
    traits::{ListenerExt, Stream as _, StreamCommon as _},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{debug, warn};

/// Maximum number of URLs accepted in a single [`ForwardPayload`].
const MAX_FORWARD_URLS: usize = 100;
/// Maximum byte length of a single forwarded URL.
const MAX_FORWARD_URL_LEN: usize = 1024;
/// Hard cap on the JSON line we will buffer from a client, with slack for the
/// surrounding `{"urls":[…]}` structure and per-entry quoting/escaping.
///
/// `MAX_FORWARD_URLS` / `MAX_FORWARD_URL_LEN` only apply *after* the whole
/// line has been read, so without this cap a client that streams bytes and
/// never sends `\n` grows the buffer until the process is OOM-killed.
const MAX_FORWARD_LINE_LEN: u64 = (MAX_FORWARD_URLS * (MAX_FORWARD_URL_LEN + 8) + 4096) as u64;
/// Read/write timeout on an accepted connection.
///
/// Without it a client that connects and never writes blocks the accept loop
/// forever, so every subsequent `buffr <url>` invocation hangs in
/// `try_forward` waiting for an ack that can never come.
const ACCEPT_IO_TIMEOUT: Duration = Duration::from_secs(5);

/// URL schemes that are safe to forward from an external process invocation.
///
/// `javascript:` and `data:` are excluded: they could be used to trigger
/// script execution or navigate to attacker-controlled content via the IPC
/// socket. Programmatic navigations from within the app are not routed
/// through this path.
const ALLOWED_FORWARD_SCHEMES: &[&str] = &[
    "http",
    "https",
    "file",
    "ftp",
    "ftps",
    "about",
    "chrome",
    "view-source",
    "mailto",
    "buffr",
];

/// Validate a single URL received over the IPC socket.
///
/// Returns `true` when the URL is safe to forward to the running instance.
fn is_safe_forward_url(url: &str) -> bool {
    if url.len() > MAX_FORWARD_URL_LEN {
        return false;
    }
    match url::Url::parse(url) {
        Ok(parsed) => ALLOWED_FORWARD_SCHEMES.contains(&parsed.scheme()),
        // Unparseable URLs are rejected.
        Err(_) => false,
    }
}

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
    /// `flock`ed file proving *we* own this profile. Kept open for the
    /// process lifetime; the kernel releases the lock when the process dies,
    /// including on SIGKILL. Never read — its existence is the point.
    #[cfg(unix)]
    #[allow(dead_code)]
    pub(crate) profile_lock: std::fs::File,
}

impl Drop for SingletonHandle {
    fn drop(&mut self) {
        #[cfg(unix)]
        if !self.socket_path.as_os_str().is_empty() {
            // Safe to unlink unconditionally: we hold the profile lock, so
            // nobody else can have replaced the socket at this path.
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

/// Our own uid.
#[cfg(unix)]
fn own_uid() -> libc::uid_t {
    // SAFETY: `getuid` is always safe; it takes no arguments and cannot fail.
    unsafe { libc::getuid() }
}

/// Create (if needed) and validate a directory only we can enter.
///
/// Rejects anything that is not a real directory owned by our uid with no
/// group/other permission bits. `symlink_metadata` — not `metadata` — so a
/// symlink planted at the path is rejected instead of silently followed.
#[cfg(unix)]
fn ensure_private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};

    match std::fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e).context(format!("creating {}", path.display())),
    }

    let md = std::fs::symlink_metadata(path).context(format!("stat {}", path.display()))?;
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

/// Private per-uid directory holding the singleton socket and lock file.
///
/// `$XDG_RUNTIME_DIR/buffr` when usable (that tree is already 0700 and
/// per-uid), otherwise `$TMPDIR/buffr-<uid>` created 0700. Both are verified
/// after creation — see [`ensure_private_dir`]. This is what stops another
/// local user from binding the (fully derivable) socket path first and
/// silently receiving every `buffr <url>` invocation's URLs.
#[cfg(unix)]
fn private_runtime_dir() -> Result<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        let p = PathBuf::from(xdg).join("buffr");
        match ensure_private_dir(&p) {
            Ok(()) => return Ok(p),
            Err(e) => warn!(
                path = %p.display(),
                error = %e,
                "single_instance: XDG_RUNTIME_DIR unusable; falling back to temp dir"
            ),
        }
    }
    let p = std::env::temp_dir().join(format!("buffr-{}", own_uid()));
    ensure_private_dir(&p)?;
    Ok(p)
}

/// Compute the socket path / pipe name for the given profile.
///
/// ## Windows
///
/// Named pipes live in a global namespace with no per-user directory to
/// harden, so the path is unchanged. Cross-user squatting is mitigated on
/// the accept side instead: `peer_creds` is checked before a payload is
/// honoured, exactly as on Unix.
fn socket_path_for(profile_id: &str) -> Result<PathBuf> {
    #[cfg(windows)]
    {
        // Named pipe path: \\.\pipe\buffr-<profile_id>
        Ok(PathBuf::from(format!(r"\\.\pipe\buffr-{profile_id}")))
    }
    #[cfg(unix)]
    {
        Ok(private_runtime_dir()?.join(format!("buffr-{profile_id}.sock")))
    }
}

/// Path of the `flock` file that decides who owns the profile.
#[cfg(unix)]
fn lock_path_for(socket_path: &Path) -> PathBuf {
    let mut p = socket_path.to_path_buf();
    p.set_extension("lock");
    p
}

/// Try to take the exclusive advisory lock for this profile.
///
/// Returns `Ok(None)` when another live process already holds it. The lock
/// is released by the kernel when our process dies — including on SIGKILL —
/// so a crashed instance never wedges the profile. The lock file itself is
/// deliberately never unlinked: unlinking a file others may have open is
/// what re-introduces the "two owners" race.
#[cfg(unix)]
fn try_lock_profile(path: &Path) -> Result<Option<std::fs::File>> {
    use std::os::unix::io::AsRawFd;

    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .context(format!("opening profile lock {}", path.display()))?;

    // SAFETY: `file` owns a valid fd for the duration of the call.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(Some(file));
    }
    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN => Ok(None),
        _ => Err(err).context(format!("flock {}", path.display())),
    }
}

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

/// Build a [`interprocess::local_socket::Name`] from a socket path.
fn make_name(path: &Path) -> Result<interprocess::local_socket::Name<'static>> {
    path.to_path_buf()
        .to_fs_name::<GenericFilePath>()
        .context("building local socket name from path")
}

/// Result of one attempt to hand our URLs to an existing instance.
enum ConnectOutcome {
    /// Payload delivered and acknowledged by a live singleton.
    Forwarded,
    /// The singleton is alive but refused the payload (`ERR` ack — e.g. the
    /// scheme gate rejected every URL) or closed without acking. The socket
    /// belongs to a live process, so this must NOT be treated as "no server".
    Rejected(String),
    /// Nothing is listening (`ENOENT` / `ECONNREFUSED`). The socket file, if
    /// one exists, is genuinely stale and may be replaced.
    Stale,
    /// Connect failed for some other reason — `EAGAIN` from a busy backlog,
    /// `EPERM`, `EINTR`, … A live singleton may well still own the socket, so
    /// this must NOT be treated as "no server": removing the socket here is
    /// what produces two processes that each believe they own the profile.
    Ambiguous(String),
}

/// Try to connect as a client and forward the URLs.
fn try_forward(path: &Path, urls: &[String]) -> Result<ConnectOutcome> {
    let name = make_name(path)?;
    let stream = match Stream::connect(name) {
        Ok(s) => s,
        Err(e) => {
            use std::io::ErrorKind::*;
            match e.kind() {
                NotFound | ConnectionRefused => {
                    debug!(error = %e, "single_instance: no server listening (stale/absent)");
                    return Ok(ConnectOutcome::Stale);
                }
                _ => {
                    debug!(error = %e, "single_instance: ambiguous connect error");
                    return Ok(ConnectOutcome::Ambiguous(e.to_string()));
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

    // Read the ack. The server sends exactly "OK\n" when it accepted the
    // payload or "ERR\n" when it rejected it (bad payload, scheme gate).
    let mut ack = String::new();
    let n = BufReader::new(&stream)
        .read_line(&mut ack)
        .context("reading ack from server")?;
    debug!(ack = %ack.trim(), "single_instance: forwarding ack received");
    Ok(classify_ack(n, &ack))
}

/// Classify the server's ack line into a [`ConnectOutcome`].
///
/// - `"OK"` → the payload was accepted → [`ConnectOutcome::Forwarded`].
/// - `"ERR"` → rejected (bad payload, or the scheme gate dropped every URL) —
///   the client must not report "forwarded".
/// - EOF / empty line (`n == 0`) → the server closed without acking — wedged
///   or died mid-handling; not success either.
/// - anything else → fail closed as `Rejected` with the raw line.
fn classify_ack(n: usize, ack: &str) -> ConnectOutcome {
    let ack = ack.trim();
    if n == 0 || ack.is_empty() {
        return ConnectOutcome::Rejected("server closed without an ack".to_string());
    }
    if ack == "OK" {
        return ConnectOutcome::Forwarded;
    }
    ConnectOutcome::Rejected(ack.to_string())
}

/// Bind the singleton listener at `path`.
///
/// On Unix this binds to a unique temporary name in the same directory and
/// then `rename(2)`s it over the target. `rename` is atomic and replaces the
/// entry in one step, so — unlike unlink-then-bind — there is never a window
/// where the socket is missing, and we never unlink a socket some other
/// process is actively listening on. The caller must already hold the
/// profile lock, which is what actually guarantees single ownership.
#[cfg(unix)]
fn try_bind(path: &Path) -> Result<interprocess::local_socket::Listener> {
    use std::os::unix::fs::PermissionsExt;

    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(".buffr-bind-{}.sock", std::process::id()));
    // A leftover from a previous crashed bind of *ours* (same pid, same
    // private dir) is the only thing that can be here.
    let _ = std::fs::remove_file(&tmp);

    let name = make_name(&tmp)?;
    let listener = ListenerOptions::new()
        .name(name)
        // We rename the socket into place ourselves; letting the listener
        // reclaim (unlink) the temp name on drop would delete the wrong entry.
        .reclaim_name(false)
        .create_sync()
        .context("binding singleton listener")?;

    if let Err(e) = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)) {
        warn!(path = %tmp.display(), error = %e, "single_instance: chmod 0600 on socket failed");
    }

    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(listener),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e).context(format!(
                "renaming singleton socket into place at {}",
                path.display()
            ))
        }
    }
}

/// Windows: named pipes cannot be renamed, and creation is already atomic
/// (a second `CreateNamedPipe` for an existing name fails), so bind directly.
#[cfg(windows)]
fn try_bind(path: &Path) -> Result<interprocess::local_socket::Listener> {
    let name = make_name(path)?;
    ListenerOptions::new()
        .name(name)
        .create_sync()
        .context("binding singleton listener")
}

/// How many times to retry an ambiguous connect before giving up.
const CONNECT_ATTEMPTS: usize = 3;
/// Backoff between ambiguous connect retries.
const CONNECT_RETRY_DELAY: Duration = Duration::from_millis(100);

/// Acquire the singleton lock.
///
/// 1. Try to **connect** as a client and forward `urls`.
///    - Success → `AcquireResult::Forwarded`.
///    - `ENOENT` / `ECONNREFUSED` → genuinely stale; fall through.
///    - Anything else → retry a few times, then fail loudly rather than
///      stealing the socket from a live-but-busy singleton.
/// 2. Take the per-profile `flock` (Unix). Contended → someone else owns the
///    profile; retry the connect and forward.
/// 3. **Bind** as listener (atomically, see [`try_bind`]).
pub fn try_acquire(profile_id: &str, urls: &[String]) -> Result<AcquireResult> {
    let path = socket_path_for(profile_id)?;
    debug!(path = %path.display(), "single_instance: acquiring singleton lock");

    // Step 1: try to forward to an existing instance.
    match forward_with_retries(&path, urls)? {
        ConnectOutcome::Forwarded => return Ok(AcquireResult::Forwarded),
        ConnectOutcome::Rejected(msg) => {
            anyhow::bail!(
                "the running buffr instance refused the forwarded URL(s) \
                 (second instance exits; nothing was opened): {msg}"
            );
        }
        ConnectOutcome::Stale => {}
        ConnectOutcome::Ambiguous(msg) => {
            anyhow::bail!(
                "could not reach the existing buffr instance on {} and will not \
                 replace its socket (last error: {msg}). If no buffr is running, \
                 remove that file and retry.",
                path.display()
            );
        }
    }

    // Step 2: claim ownership of the profile before touching the socket file.
    #[cfg(unix)]
    let profile_lock = {
        let lock_path = lock_path_for(&path);
        match try_lock_profile(&lock_path)? {
            Some(f) => f,
            None => {
                // Another live process owns the profile; it just wasn't
                // accepting when we tried. Give it another chance.
                debug!(path = %lock_path.display(), "single_instance: profile lock held by another process");
                return match forward_with_retries(&path, urls)? {
                    ConnectOutcome::Forwarded => Ok(AcquireResult::Forwarded),
                    other => {
                        let detail = match other {
                            ConnectOutcome::Ambiguous(msg) | ConnectOutcome::Rejected(msg) => msg,
                            _ => "socket not listening".to_string(),
                        };
                        Err(anyhow::anyhow!(
                            "another buffr process owns this profile but is not accepting \
                             connections ({detail}); refusing to start a second instance"
                        ))
                    }
                };
            }
        }
    };

    // Step 3: bind.
    let listener = try_bind(&path)?;
    debug!(path = %path.display(), "single_instance: we are the singleton");
    Ok(AcquireResult::Owner(SingletonHandle {
        listener,
        socket_path: path,
        #[cfg(unix)]
        profile_lock,
    }))
}

/// Connect + forward, retrying only the ambiguous failures.
fn forward_with_retries(path: &Path, urls: &[String]) -> Result<ConnectOutcome> {
    let mut last = ConnectOutcome::Stale;
    for attempt in 1..=CONNECT_ATTEMPTS {
        match try_forward(path, urls)? {
            ConnectOutcome::Forwarded => return Ok(ConnectOutcome::Forwarded),
            // Rejected/Stale are final — a refusal or an absent server is not
            // going to resolve by retrying.
            ConnectOutcome::Rejected(msg) => return Ok(ConnectOutcome::Rejected(msg)),
            ConnectOutcome::Stale => return Ok(ConnectOutcome::Stale),
            ConnectOutcome::Ambiguous(msg) => {
                last = ConnectOutcome::Ambiguous(msg);
                if attempt < CONNECT_ATTEMPTS {
                    std::thread::sleep(CONNECT_RETRY_DELAY);
                }
            }
        }
    }
    Ok(last)
}

/// Outcome of [`read_capped_line`].
#[derive(Debug, PartialEq, Eq)]
enum CappedLine {
    /// A complete newline-terminated line (the trailing `\n` is included).
    Line(String),
    /// The peer closed the connection without sending anything.
    Eof,
    /// The peer sent [`MAX_FORWARD_LINE_LEN`] bytes without a newline, or
    /// closed mid-line. Nothing usable; reject.
    TooLong,
}

/// Read one newline-terminated line, refusing to buffer more than
/// [`MAX_FORWARD_LINE_LEN`] bytes.
///
/// A plain `read_line` into a `String` is unbounded: the per-URL caps only
/// apply once the whole line has been buffered, so a client that streams
/// bytes and never sends `\n` grows the allocation until the process dies.
fn read_capped_line<R: Read>(reader: R) -> std::io::Result<CappedLine> {
    let mut line = String::new();
    let n = BufReader::new(reader.take(MAX_FORWARD_LINE_LEN)).read_line(&mut line)?;
    if n == 0 {
        return Ok(CappedLine::Eof);
    }
    if !line.ends_with('\n') {
        return Ok(CappedLine::TooLong);
    }
    Ok(CappedLine::Line(line))
}

/// Is the peer on this connection running as our own uid?
///
/// Uses `SO_PEERCRED` on Linux and `getpeereid` on the BSDs/macOS, via
/// `interprocess`'s `peer_creds`. A connection whose credentials cannot be
/// determined at all is accepted with a warning rather than dropped — some
/// platforms simply do not report a uid, and refusing there would break
/// single-instance forwarding entirely.
#[cfg(unix)]
fn peer_is_us(stream: &Stream) -> bool {
    let creds = match stream.peer_creds() {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "single_instance: could not read peer credentials; rejecting");
            return false;
        }
    };
    match creds.euid() {
        Some(uid) if uid == own_uid() => true,
        Some(uid) => {
            warn!(
                peer_uid = uid,
                our_uid = own_uid(),
                "single_instance: rejecting forwarded payload from another user"
            );
            false
        }
        None => {
            warn!("single_instance: platform does not report a peer uid; accepting");
            true
        }
    }
}

/// Windows: named pipes live in a global namespace, so verify the client is
/// the same user before honouring anything it sends.
#[cfg(windows)]
fn peer_is_us(stream: &Stream) -> bool {
    // `interprocess` exposes only the peer pid on Windows. Impersonation-based
    // uid checks are out of scope here; the connection is accepted but the
    // payload still goes through the scheme allow-list and length caps below.
    match stream.peer_creds() {
        Ok(creds) => {
            debug!(peer_pid = ?creds.pid(), "single_instance: accepted client");
            true
        }
        Err(e) => {
            warn!(error = %e, "single_instance: could not read peer credentials; accepting");
            true
        }
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
    proxy: crate::windowing::EventLoopProxy<crate::BuffrUserEvent>,
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

                // Only our own uid may hand us URLs. Without this a peer that
                // won the race for the socket/pipe name could feed navigation
                // targets into a running browser.
                if !peer_is_us(&stream) {
                    let _ = stream.write_all(b"ERR\n");
                    continue;
                }

                // A client that connects and never writes must not wedge the
                // accept loop — every later `buffr <url>` would hang.
                if let Err(e) = stream.set_recv_timeout(Some(ACCEPT_IO_TIMEOUT)) {
                    warn!(error = %e, "single_instance: could not set recv timeout (continuing)");
                }
                if let Err(e) = stream.set_send_timeout(Some(ACCEPT_IO_TIMEOUT)) {
                    warn!(error = %e, "single_instance: could not set send timeout (continuing)");
                }

                // Read one newline-terminated JSON line, hard-capped.
                let line = match read_capped_line(&stream) {
                    Ok(CappedLine::Line(l)) => l,
                    Ok(CappedLine::Eof) => {
                        warn!("single_instance: client closed before sending payload");
                        continue;
                    }
                    Ok(CappedLine::TooLong) => {
                        warn!(
                            cap = MAX_FORWARD_LINE_LEN,
                            "single_instance: payload exceeded the line cap or ended without \
                             a newline; dropping"
                        );
                        let _ = stream.write_all(b"ERR\n");
                        continue;
                    }
                    Err(e) => {
                        warn!(error = %e, "single_instance: read error from client (continuing)");
                        continue;
                    }
                };
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
                // Enforce caps and scheme allow-list before forwarding.
                let capped: Vec<String> = payload.urls.into_iter().take(MAX_FORWARD_URLS).collect();
                let safe_urls: Vec<String> = capped
                    .into_iter()
                    .filter(|u| {
                        if is_safe_forward_url(u) {
                            true
                        } else {
                            warn!(url = %u, "single_instance: dropping forwarded URL with disallowed scheme or length");
                            false
                        }
                    })
                    .collect();
                if safe_urls.is_empty() {
                    warn!("single_instance: all forwarded URLs were rejected — skipping event");
                    // ERR, not OK: nothing was accepted, and the client must
                    // not report "forwarded" when it is (a) about to exit and
                    // (b) leaving the user with nothing opened.
                    let _ = stream.write_all(b"ERR\n");
                    continue;
                }
                // Send event to the winit loop.
                if let Err(e) = proxy.send_event(crate::BuffrUserEvent::OpenUrls(safe_urls)) {
                    warn!(error = %e, "single_instance: proxy.send_event failed (loop closed?)");
                }
                // Ack.
                let _ = stream.write_all(b"OK\n");
            }
            debug!("single_instance: accept thread exiting");
        })
        .expect("single_instance: failed to spawn accept thread");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::{
        ConnectOutcome, MAX_FORWARD_URL_LEN, classify_ack, is_safe_forward_url, profile_id_from,
    };

    // ---- Group 1: ack classification -----------------------------------------
    //
    // The client must not report "forwarded" unless the server actually took
    // the payload. Regression: the old code returned Forwarded on any line
    // including EOF, so `buffr <rejected-url>` with a live singleton silently
    // "succeeded" while nothing was opened.

    #[test]
    fn ok_ack_is_forwarded() {
        assert!(matches!(classify_ack(3, "OK\n"), ConnectOutcome::Forwarded));
    }

    #[test]
    fn err_ack_is_rejected() {
        assert!(matches!(
            classify_ack(4, "ERR\n"),
            ConnectOutcome::Rejected(_)
        ));
    }

    #[test]
    fn eof_without_ack_is_rejected() {
        assert!(matches!(classify_ack(0, ""), ConnectOutcome::Rejected(_)));
    }

    #[test]
    fn empty_line_is_rejected() {
        assert!(matches!(classify_ack(1, "\n"), ConnectOutcome::Rejected(_)));
    }

    #[test]
    fn unknown_ack_fails_closed() {
        assert!(matches!(
            classify_ack(4, "??\n"),
            ConnectOutcome::Rejected(_)
        ));
    }

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
        let long: String = "a".repeat(4090);
        let path = format!("/tmp/{long}");
        let id = profile_id_from(&path);
        assert_eq!(id.len(), 16);
        assert!(
            id.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')),
            "non-hex char in long-path id: {id}"
        );
    }

    // ── Security: IPC URL validation ─────────────────────────────────────────

    #[test]
    fn safe_forward_url_allows_http_and_https() {
        assert!(is_safe_forward_url("https://example.com/page"));
        assert!(is_safe_forward_url("http://localhost:3000"));
        assert!(is_safe_forward_url("http://192.168.1.1:8080/path"));
    }

    #[test]
    fn safe_forward_url_rejects_javascript_scheme() {
        assert!(
            !is_safe_forward_url("javascript:alert(1)"),
            "javascript: must be rejected"
        );
        assert!(!is_safe_forward_url("javascript:void(0)"));
    }

    #[test]
    fn safe_forward_url_rejects_data_scheme() {
        assert!(
            !is_safe_forward_url("data:text/html,<script>xss</script>"),
            "data: must be rejected"
        );
    }

    #[test]
    fn safe_forward_url_rejects_overlong_urls() {
        let long = format!("https://example.com/{}", "a".repeat(MAX_FORWARD_URL_LEN));
        assert!(
            !is_safe_forward_url(&long),
            "URLs over MAX_FORWARD_URL_LEN must be rejected"
        );
    }

    #[test]
    fn safe_forward_url_rejects_unparseable() {
        assert!(!is_safe_forward_url("not a url at all"));
        assert!(!is_safe_forward_url(""));
    }

    #[test]
    fn safe_forward_url_allows_file_and_mailto() {
        assert!(is_safe_forward_url("file:///etc/hosts"));
        assert!(is_safe_forward_url("mailto:user@example.com"));
    }

    // ── Security: bounded IPC line reads (M10) ───────────────────────────────

    use super::{CappedLine, MAX_FORWARD_LINE_LEN, read_capped_line};

    #[test]
    fn read_capped_line_returns_a_complete_line() {
        let data = b"{\"urls\":[\"https://example.com\"]}\nleftover";
        assert_eq!(
            read_capped_line(&data[..]).unwrap(),
            CappedLine::Line("{\"urls\":[\"https://example.com\"]}\n".to_string())
        );
    }

    #[test]
    fn read_capped_line_reports_eof_on_an_empty_stream() {
        assert_eq!(read_capped_line(&b""[..]).unwrap(), CappedLine::Eof);
    }

    #[test]
    fn read_capped_line_rejects_a_line_without_a_newline() {
        // Client closed mid-line: nothing usable.
        assert_eq!(
            read_capped_line(&b"{\"urls\":["[..]).unwrap(),
            CappedLine::TooLong
        );
    }

    /// A client that streams bytes and never sends `\n` must be cut off at
    /// the cap instead of growing the buffer until the process is OOM-killed.
    #[test]
    fn read_capped_line_stops_at_the_cap() {
        struct Endless;
        impl std::io::Read for Endless {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                buf.fill(b'a');
                Ok(buf.len())
            }
        }
        assert_eq!(read_capped_line(Endless).unwrap(), CappedLine::TooLong);
        // Sanity: the cap is big enough for a maximal legitimate payload.
        assert!(
            MAX_FORWARD_LINE_LEN as usize > super::MAX_FORWARD_URLS * super::MAX_FORWARD_URL_LEN
        );
    }

    // ── Security: private runtime directory + profile lock (H13/H14) ─────────

    #[cfg(unix)]
    mod unix {
        use super::super::{ensure_private_dir, lock_path_for, try_bind, try_lock_profile};
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        #[test]
        fn ensure_private_dir_creates_0700_and_is_idempotent() {
            let base = tempfile::tempdir().expect("tempdir");
            let p = base.path().join("buffr-runtime");
            ensure_private_dir(&p).expect("first create");
            let md = std::fs::symlink_metadata(&p).unwrap();
            assert_eq!(md.mode() & 0o7777, 0o700);
            ensure_private_dir(&p).expect("second call");
        }

        #[test]
        fn ensure_private_dir_rejects_group_or_world_accessible_dirs() {
            let base = tempfile::tempdir().expect("tempdir");
            let p = base.path().join("loose");
            std::fs::create_dir(&p).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o777)).unwrap();
            let err = ensure_private_dir(&p).expect_err("0777 must be rejected");
            assert!(
                err.to_string().contains("group/other"),
                "unexpected error: {err}"
            );
        }

        #[test]
        fn ensure_private_dir_rejects_a_symlink() {
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

        /// H13: exactly one holder at a time. `flock` locks are attached to
        /// the open file description, so two independent opens conflict even
        /// inside the same process.
        #[test]
        fn profile_lock_is_exclusive_and_released_on_drop() {
            let base = tempfile::tempdir().expect("tempdir");
            let path = base.path().join("profile.lock");

            let first = try_lock_profile(&path).unwrap();
            assert!(first.is_some(), "first acquisition must succeed");

            let second = try_lock_profile(&path).unwrap();
            assert!(
                second.is_none(),
                "a second holder must be refused while the lock is held"
            );

            drop(first);
            let third = try_lock_profile(&path).unwrap();
            assert!(third.is_some(), "lock must be reusable after release");
        }

        #[test]
        fn lock_path_sits_next_to_the_socket() {
            let p = lock_path_for(std::path::Path::new("/run/user/1000/buffr/buffr-ab.sock"));
            assert_eq!(
                p,
                std::path::PathBuf::from("/run/user/1000/buffr/buffr-ab.lock")
            );
        }

        /// The socket must land at the requested path (via the temp-name +
        /// `rename` dance) with owner-only permissions, and no temp artefact
        /// may be left behind.
        #[test]
        fn try_bind_renames_a_0600_socket_into_place() {
            let base = tempfile::tempdir().expect("tempdir");
            let sock = base.path().join("buffr-test.sock");

            let listener = try_bind(&sock).expect("bind");
            let md = std::fs::symlink_metadata(&sock).expect("socket must exist at target path");
            assert_eq!(md.mode() & 0o777, 0o600, "socket must be owner-only");

            let leftovers: Vec<_> = std::fs::read_dir(base.path())
                .unwrap()
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with(".buffr-bind-"))
                .collect();
            assert!(
                leftovers.is_empty(),
                "temporary bind socket was not renamed away: {leftovers:?}"
            );

            drop(listener);
        }
    }
}
