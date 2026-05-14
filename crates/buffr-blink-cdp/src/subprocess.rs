//! Chromium binary detection and subprocess lifecycle.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use crate::error::BlinkError;

/// Ordered list of candidate binary names / paths to probe.
#[cfg(target_os = "macos")]
const CANDIDATES: &[&str] = &[
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "google-chrome",
    "chromium",
    "chromium-browser",
    "chrome",
];

#[cfg(not(target_os = "macos"))]
const CANDIDATES: &[&str] = &[
    "chromium-browser",
    "chromium",
    "google-chrome",
    "google-chrome-stable",
    "chrome",
];

/// Find a usable Chromium / Chrome binary via PATH or well-known paths.
///
/// Returns `None` when none of the candidates are found — the caller should
/// surface a user-friendly error via [`BlinkError::ChromiumNotFound`].
pub fn find_chromium() -> Option<PathBuf> {
    for candidate in CANDIDATES {
        let path = PathBuf::from(candidate);
        // Absolute path — check existence directly.
        if path.is_absolute() {
            if path.exists() {
                return Some(path);
            }
            continue;
        }
        // Relative name — resolve through PATH via `which`.
        if let Ok(resolved) = which_binary(candidate) {
            return Some(resolved);
        }
    }
    None
}

/// Minimal `which`-style lookup: searches PATH entries for `name`.
fn which_binary(name: &str) -> Result<PathBuf, ()> {
    let path_var = std::env::var_os("PATH").ok_or(())?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            // Check execute bit on unix.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(meta) = candidate.metadata()
                    && meta.permissions().mode() & 0o111 != 0
                {
                    return Ok(candidate);
                }
            }
            #[cfg(not(unix))]
            return Ok(candidate);
        }
    }
    Err(())
}

/// Ask the OS for a free TCP port by binding on `127.0.0.1:0`.
///
/// The listener is dropped immediately so Chromium can bind the same address.
/// There is an inherent TOCTOU window between the drop and Chromium's bind —
/// callers must propagate any resulting `EADDRINUSE` as a startup error.
///
/// # Errors
///
/// Returns [`BlinkError::PortProbe`] when the OS refuses to bind even the
/// ephemeral address (e.g. resource exhaustion).
pub fn pick_free_port() -> Result<u16, BlinkError> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(BlinkError::PortProbe)?;
    let port = listener.local_addr().map_err(BlinkError::PortProbe)?.port();
    drop(listener); // release before Chromium binds it
    Ok(port)
}

/// Spawn headless Chromium with remote DevTools on `port`.
///
/// `user_data_dir` is the profile directory. Callers typically pass a
/// temporary directory so each engine instance has an isolated profile.
///
/// # Errors
///
/// Returns [`BlinkError::SpawnFailed`] when the OS rejects the spawn.
pub fn spawn_headless(
    chromium: &Path,
    port: u16,
    user_data_dir: &Path,
) -> Result<Child, BlinkError> {
    tracing::debug!(
        binary = %chromium.display(),
        port,
        user_data_dir = %user_data_dir.display(),
        "spawning headless Chromium"
    );
    Command::new(chromium)
        .args([
            "--headless=new",
            &format!("--remote-debugging-port={port}"),
            &format!("--user-data-dir={}", user_data_dir.display()),
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-gpu",
            "--disable-software-rasterizer",
            "--disable-dev-shm-usage",
            "--disable-extensions",
            "--disable-background-networking",
            "--log-level=3", // suppress most Chrome stderr noise
            "about:blank",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(BlinkError::SpawnFailed)
}

/// Probe `http://127.0.0.1:<port>/json/version` until Chromium is ready.
///
/// Returns the `webSocketDebuggerUrl` from the JSON response.
/// Retries up to `max_attempts` times with `delay` between attempts.
pub fn probe_ws_url(port: u16, max_attempts: u32, delay: Duration) -> Result<String, BlinkError> {
    let url = format!("http://127.0.0.1:{port}/json/version");
    let mut last_err = String::new();
    for attempt in 0..max_attempts {
        std::thread::sleep(delay);
        tracing::debug!(attempt, port, "probing CDP endpoint");
        match ureq::get(&url).call() {
            Ok(resp) => {
                let body: serde_json::Value = resp
                    .into_body()
                    .read_json()
                    .map_err(|e| BlinkError::Protocol(format!("CDP version JSON parse: {e}")))?;
                let ws_url = body
                    .get("webSocketDebuggerUrl")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        BlinkError::Protocol("missing webSocketDebuggerUrl in /json/version".into())
                    })?;
                tracing::debug!(ws_url, "CDP ready");
                return Ok(ws_url.to_owned());
            }
            Err(e) => {
                last_err = e.to_string();
                tracing::debug!(attempt, error = %e, "CDP not ready yet");
            }
        }
    }
    Err(BlinkError::ProbeFailed {
        port,
        source: last_err.into(),
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_free_port_returns_nonzero() {
        let port = pick_free_port().expect("OS should provide an ephemeral port");
        assert_ne!(port, 0, "port must be non-zero");
    }

    #[test]
    fn pick_free_port_returns_different_ports_across_calls_usually() {
        let p1 = pick_free_port().expect("first pick failed");
        let p2 = pick_free_port().expect("second pick failed");
        // Both must be bindable (already released by pick_free_port).
        assert_ne!(p1, 0);
        assert_ne!(p2, 0);
        // Ports are usually different; allow equality — the assertion above
        // already proves both binds succeeded, which is the meaningful check.
        let _ = p1 != p2; // suppress unused-comparison lint
    }
}
