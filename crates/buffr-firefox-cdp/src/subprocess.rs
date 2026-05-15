//! Firefox binary detection and subprocess lifecycle.
//!
//! # Key differences from blink-cdp
//!
//! - Firefox requires a pre-initialized profile directory with `prefs.js` so
//!   the Remote Agent / CDP endpoint is enabled and the first-run wizard is
//!   suppressed.
//! - Launch flags differ significantly: `--no-remote`, `--headless`,
//!   `--remote-debugging-port`, `--profile`. No `--user-data-dir`,
//!   no `--disable-gpu`.
//! - `Page.startScreencast` is NOT available in Firefox CDP. The OSR layer
//!   uses a `Page.captureScreenshot` poll loop instead.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use crate::error::FirefoxError;

/// Maximum number of attempts when probing the CDP `/json/version` endpoint.
///
/// Firefox startup is slower than Chromium (~800 ms cold). 30 × 300 ms = 9 s
/// total budget covers most startup delays on a lightly-loaded host.
pub const WS_PROBE_MAX_RETRIES: u32 = 30;

/// Delay between successive CDP endpoint probe attempts.
pub const WS_PROBE_INTERVAL_MS: u64 = 300;

/// Ordered candidate binary names / paths — Linux.
#[cfg(not(target_os = "macos"))]
const CANDIDATES: &[&str] = &[
    "firefox",
    "firefox-developer-edition",
    "firefox-esr",
    "firefox-nightly",
];

/// Ordered candidate binary names / paths — macOS.
#[cfg(target_os = "macos")]
const CANDIDATES: &[&str] = &[
    "/Applications/Firefox.app/Contents/MacOS/firefox",
    "/Applications/Firefox Developer Edition.app/Contents/MacOS/firefox",
    "firefox",
    "firefox-developer-edition",
    "firefox-esr",
    "firefox-nightly",
];

/// Find a usable Firefox binary via well-known paths or PATH.
///
/// Returns `None` when none of the candidates can be resolved — the caller
/// should surface a user-friendly error via [`FirefoxError::FirefoxNotFound`].
pub fn find_firefox() -> Option<PathBuf> {
    find_firefox_from_candidates(CANDIDATES)
}

/// Inner implementation that accepts an explicit candidate list.
///
/// Separated from [`find_firefox`] so tests can supply a controlled set of
/// candidates without mutating the process environment.
pub fn find_firefox_from_candidates(candidates: &[&str]) -> Option<PathBuf> {
    for candidate in candidates {
        let path = PathBuf::from(candidate);
        if path.is_absolute() {
            if path.exists() {
                return Some(path);
            }
            continue;
        }
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
/// The listener is dropped immediately so Firefox can bind the same address.
/// There is an inherent TOCTOU window between the drop and Firefox's bind —
/// callers must propagate any resulting `EADDRINUSE` as a startup error.
pub fn pick_free_port() -> Result<u16, FirefoxError> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(FirefoxError::PortProbe)?;
    let port = listener
        .local_addr()
        .map_err(FirefoxError::PortProbe)?
        .port();
    drop(listener);
    Ok(port)
}

/// Minimal Firefox `prefs.js` written into the profile directory before launch.
///
/// These preferences enable the Remote Agent (CDP endpoint), suppress the
/// first-run setup wizard, and disable telemetry prompts that would block
/// headless operation.
const PREFS_JS: &str = r#"user_pref("devtools.chrome.enabled", true);
user_pref("devtools.debugger.remote-enabled", true);
user_pref("devtools.debugger.prompt-connection", false);
user_pref("remote.enabled", true);
user_pref("remote.force-local", true);
user_pref("browser.shell.checkDefaultBrowser", false);
user_pref("browser.startup.homepage_override.mstone", "ignore");
user_pref("toolkit.telemetry.reportingpolicy.firstRun", false);
"#;

/// Write the minimal `prefs.js` into `profile_dir`.
///
/// If the file already exists it is overwritten so preferences stay current.
fn write_prefs(profile_dir: &Path) -> Result<(), FirefoxError> {
    let prefs_path = profile_dir.join("prefs.js");
    std::fs::write(&prefs_path, PREFS_JS).map_err(FirefoxError::SpawnFailed)
}

/// Initialize a Firefox profile directory: create it if absent and write
/// the required `prefs.js`.
pub fn init_profile(profile_dir: &Path) -> Result<(), FirefoxError> {
    std::fs::create_dir_all(profile_dir).map_err(FirefoxError::ProfileDirCreate)?;
    write_prefs(profile_dir)?;
    Ok(())
}

/// Spawn headless Firefox with Remote Agent on `port`.
///
/// `profile_dir` must already exist and contain a valid `prefs.js`; call
/// [`init_profile`] first.
///
/// If `private` is `true`, adds `--private-window` to the argument list.
pub fn spawn_headless(
    firefox: &Path,
    port: u16,
    profile_dir: &Path,
    private: bool,
) -> Result<Child, FirefoxError> {
    tracing::debug!(
        binary = %firefox.display(),
        port,
        profile_dir = %profile_dir.display(),
        private,
        "spawning headless Firefox"
    );

    let mut args = vec![
        "--no-remote".to_owned(),
        "--headless".to_owned(),
        format!("--remote-debugging-port={port}"),
        "--profile".to_owned(),
        profile_dir.to_string_lossy().into_owned(),
    ];

    if private {
        args.push("--private-window".to_owned());
    }

    Command::new(firefox)
        .args(&args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(FirefoxError::SpawnFailed)
}

/// Probe `http://127.0.0.1:<port>/json/version` until Firefox is ready.
///
/// Returns the `webSocketDebuggerUrl` from the JSON response.
/// Retries up to `max_attempts` times with `delay` between attempts.
pub fn probe_ws_url(port: u16, max_attempts: u32, delay: Duration) -> Result<String, FirefoxError> {
    let url = format!("http://127.0.0.1:{port}/json/version");
    let mut last_err = String::new();
    for attempt in 0..max_attempts {
        std::thread::sleep(delay);
        tracing::debug!(attempt, port, "probing Firefox CDP endpoint");
        match ureq::get(&url).call() {
            Ok(resp) => {
                let body: serde_json::Value = resp
                    .into_body()
                    .read_json()
                    .map_err(|e| FirefoxError::Protocol(format!("CDP version JSON parse: {e}")))?;
                let ws_url = body
                    .get("webSocketDebuggerUrl")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        FirefoxError::Protocol(
                            "missing webSocketDebuggerUrl in /json/version".into(),
                        )
                    })?;
                tracing::debug!(ws_url, "Firefox CDP ready");
                return Ok(ws_url.to_owned());
            }
            Err(e) => {
                last_err = e.to_string();
                tracing::debug!(attempt, error = %e, "Firefox CDP not ready yet");
            }
        }
    }
    Err(FirefoxError::ProbeFailed {
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
    fn pick_free_port_within_ephemeral_range() {
        let port = pick_free_port().expect("OS should provide an ephemeral port");
        assert!(port >= 1024, "expected ephemeral port >= 1024, got {port}");
    }

    #[test]
    fn pick_free_port_returns_multiple_valid_ports() {
        let p1 = pick_free_port().expect("first pick failed");
        let p2 = pick_free_port().expect("second pick failed");
        assert_ne!(p1, 0);
        assert_ne!(p2, 0);
    }

    #[test]
    fn find_firefox_returns_none_when_no_candidates() {
        let result = find_firefox_from_candidates(&[]);
        assert!(
            result.is_none(),
            "empty candidate slice must yield None, got {result:?}"
        );
    }

    #[test]
    fn find_firefox_returns_none_for_nonexistent_relative_candidates() {
        let result =
            find_firefox_from_candidates(&["__buffr_test_nonexistent_firefox_binary_xyz_9a3f__"]);
        assert!(
            result.is_none(),
            "nonexistent binary must yield None, got {result:?}"
        );
    }

    #[test]
    fn init_profile_creates_prefs_js() {
        let dir = tempfile::tempdir().expect("temp dir");
        let profile = dir.path().join("ffprofile");
        init_profile(&profile).expect("init_profile failed");

        // prefs.js must exist and contain the Remote Agent pref.
        let contents = std::fs::read_to_string(profile.join("prefs.js")).expect("read prefs.js");
        assert!(
            contents.contains("devtools.debugger.remote-enabled"),
            "prefs.js missing remote-enabled: {contents}"
        );
        assert!(
            contents.contains("remote.enabled"),
            "prefs.js missing remote.enabled: {contents}"
        );
    }

    #[test]
    fn init_profile_idempotent() {
        let dir = tempfile::tempdir().expect("temp dir");
        let profile = dir.path().join("ffprofile");
        // Call twice — should succeed both times.
        init_profile(&profile).expect("first call");
        init_profile(&profile).expect("second call idempotent");
    }
}
