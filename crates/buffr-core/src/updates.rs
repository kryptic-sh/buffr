//! Phase 6 update channel: version-check + manual-update flow.
//!
//! No automatic binary replacement — that needs signing infrastructure
//! that is post-1.0 work. This module:
//!
//! 1. Hits the GitHub releases API (`api.github.com/repos/{repo}/releases/latest`)
//!    once per `check_interval_hours` to learn the latest tag.
//! 2. Writes a tiny on-disk cache so the statusline can render an
//!    indicator without re-querying the network on every launch.
//! 3. Exposes a [`HttpClient`] trait so tests can drive the state
//!    machine without touching the real network.
//!
//! Network surface: **one** GET per check interval. No headers contain
//! PII (we send only a `User-Agent: buffr/{version}`). When
//! `UpdateConfig::enabled = false` the type does **zero** network IO —
//! every entry point short-circuits to [`UpdateStatus::Disabled`].
//!
//! Dismissing a release filters at *query time*, not write time: the
//! cache is the source of truth for "what GitHub last reported" and
//! `dismissed_versions` is a separate list. That way the user can
//! always re-run `--check-for-updates` and see the same release flag
//! up; subsequent reads (`check_cached`) honor the dismiss.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Utc};
use semver::Version;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Default GitHub repo polled for releases. Users may point at a fork
/// via `[updates] github_repo = "..."`.
pub const DEFAULT_GITHUB_REPO: &str = "kryptic-sh/buffr";

/// Default check interval in hours. Once-a-day is plenty; the cache
/// soaks shorter restarts so a noisy `buffr` quitter doesn't spam.
pub const DEFAULT_CHECK_INTERVAL_HOURS: u32 = 24;

/// Default release channel. `nightly` reserved for the post-1.0
/// pre-release tag stream; today only `stable` resolves cleanly.
pub const DEFAULT_CHANNEL: &str = "stable";

/// User-Agent set on every GitHub API request. GitHub rejects requests
/// without one, so this is mandatory.
const USER_AGENT: &str = concat!("buffr/", env!("CARGO_PKG_VERSION"));

// `UpdateConfig` is defined in `buffr-config` (the [updates] section
// of the user TOML) and re-exported here for convenience.
pub use buffr_config::UpdateConfig;

/// One release as reported by the GitHub releases API, projected onto
/// the fields buffr cares about. Stored verbatim in the on-disk cache
/// so the next launch can render `* upd` without a network call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseInfo {
    pub version: Version,
    pub tag: String,
    pub published_at: DateTime<Utc>,
    pub url: String,
    /// Release body (changelog excerpt). May be empty.
    pub body: String,
}

/// Result of a check. Surfaces both fresh and cached state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateStatus {
    /// Running version is at or ahead of the latest release.
    UpToDate { current: Version },
    /// A newer release exists.
    Available {
        current: Version,
        latest: ReleaseInfo,
    },
    /// `[updates] enabled = false`. No network hit; no cache read.
    Disabled,
    /// Network or parse error. The string is a one-line summary safe
    /// to surface in the UI.
    NetworkError(String),
    /// Cache exists but is older than `check_interval_hours`. The
    /// statusline renders this as `* upd?` (the `?` is the "we don't
    /// know if this is still current" hint).
    Stale {
        last_checked: DateTime<Utc>,
        latest: ReleaseInfo,
    },
}

#[derive(Debug, Error)]
pub enum UpdateError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid current version {0:?}: {1}")]
    BadCurrent(String, semver::Error),
}

/// Pluggable HTTP backend. Tests inject a mock; production uses
/// [`UreqClient`].
pub trait HttpClient: Send + Sync {
    /// GET `url` and return the response body as a UTF-8 string. Any
    /// non-2xx response or transport error returns `Err(message)`. The
    /// string is surfaced verbatim through [`UpdateStatus::NetworkError`].
    fn get(&self, url: &str) -> Result<String, String>;
}

/// Production `HttpClient` backed by the workspace `ureq`. Pinned
/// 5s connect / 5s read timeout — the GitHub API is fast, and we
/// don't want a stalled response to wedge the chrome renderer.
pub struct UreqClient;

impl HttpClient for UreqClient {
    fn get(&self, url: &str) -> Result<String, String> {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(5))
            .timeout_read(Duration::from_secs(5))
            .user_agent(USER_AGENT)
            .build();
        let resp = agent
            .get(url)
            .set("Accept", "application/vnd.github+json")
            .call()
            .map_err(|e| format!("ureq: {e}"))?;
        resp.into_string().map_err(|e| format!("body read: {e}"))
    }
}

/// On-disk cache shape. Serialized as pretty JSON.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct CacheFile {
    /// Wall-clock instant of the last successful network query.
    last_checked: Option<DateTime<Utc>>,
    /// Latest release as reported by the most recent network query.
    latest: Option<ReleaseInfo>,
    /// Versions the user has dismissed. Filtered at read time.
    dismissed: Vec<Version>,
}

/// GitHub releases API row. Only the fields buffr reads.
#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    published_at: DateTime<Utc>,
    html_url: String,
    body: Option<String>,
}

/// Synchronous version-check + cache reader.
///
/// `check_now` performs network IO; `check_cached` reads the cache
/// only and never touches the network. `dismiss` records a version in
/// the cache as "ignored" — subsequent calls treat it as up-to-date.
pub struct UpdateChecker {
    config: UpdateConfig,
    cache_path: PathBuf,
    client: Box<dyn HttpClient>,
    /// Owning the running version up front means tests can construct
    /// a checker for any `current` without building a fake binary.
    current: Version,
    /// Cache is read-modify-written behind a mutex so concurrent
    /// `dismiss` + `check_now` from different threads don't race.
    cache_lock: Mutex<()>,
}

impl UpdateChecker {
    /// Construct with the production [`UreqClient`] and the buffr
    /// crate's compile-time version.
    pub fn new(config: UpdateConfig, cache_path: PathBuf) -> Self {
        // `env!("CARGO_PKG_VERSION")` for the `buffr-core` crate. The
        // workspace version is shared so this matches `buffr` itself.
        let current =
            Version::parse(env!("CARGO_PKG_VERSION")).unwrap_or_else(|_| Version::new(0, 0, 0));
        Self::with_client_and_current(config, cache_path, Box::new(UreqClient), current)
    }

    /// Test-friendly constructor.
    pub fn with_client_and_current(
        config: UpdateConfig,
        cache_path: PathBuf,
        client: Box<dyn HttpClient>,
        current: Version,
    ) -> Self {
        Self {
            config,
            cache_path,
            client,
            current,
            cache_lock: Mutex::new(()),
        }
    }

    /// Hits the network; updates the cache; returns the resolved
    /// status. Honors `enabled = false` (returns [`UpdateStatus::Disabled`]
    /// without any IO).
    pub fn check_now(&self) -> UpdateStatus {
        if !self.config.enabled {
            return UpdateStatus::Disabled;
        }
        let url = format!(
            "https://api.github.com/repos/{}/releases/latest",
            self.config.github_repo
        );
        let body = match self.client.get(&url) {
            Ok(b) => b,
            Err(e) => return UpdateStatus::NetworkError(e),
        };
        let parsed: GithubRelease = match serde_json::from_str(&body) {
            Ok(p) => p,
            Err(e) => return UpdateStatus::NetworkError(format!("parse: {e}")),
        };
        let tag = parsed.tag_name.trim_start_matches('v').to_string();
        let version = match Version::parse(&tag) {
            Ok(v) => v,
            Err(e) => {
                return UpdateStatus::NetworkError(format!(
                    "tag {:?} not semver: {e}",
                    parsed.tag_name
                ));
            }
        };
        let release = ReleaseInfo {
            version: version.clone(),
            tag: parsed.tag_name,
            published_at: parsed.published_at,
            url: parsed.html_url,
            body: parsed.body.unwrap_or_default(),
        };
        // Persist before resolving so a concurrent `check_cached`
        // sees the new state.
        let _guard = self.cache_lock.lock().ok();
        let mut cache = read_cache(&self.cache_path).unwrap_or_default();
        cache.last_checked = Some(Utc::now());
        cache.latest = Some(release.clone());
        let _ = write_cache(&self.cache_path, &cache);
        self.resolve_status(&release, &cache.dismissed, false, cache.last_checked)
    }

    /// Read the on-disk cache only. Returns [`UpdateStatus::Stale`]
    /// when the cache is older than `check_interval_hours`. Returns
    /// [`UpdateStatus::Disabled`] when `enabled = false`. When no
    /// cache exists, returns [`UpdateStatus::UpToDate`] for the
    /// running version (no point flagging "we don't know").
    pub fn check_cached(&self) -> UpdateStatus {
        if !self.config.enabled {
            return UpdateStatus::Disabled;
        }
        let cache = match read_cache(&self.cache_path) {
            Ok(c) => c,
            Err(_) => {
                return UpdateStatus::UpToDate {
                    current: self.current.clone(),
                };
            }
        };
        let Some(release) = cache.latest.clone() else {
            return UpdateStatus::UpToDate {
                current: self.current.clone(),
            };
        };
        let last_checked = cache.last_checked;
        let stale = match last_checked {
            Some(ts) => self.is_stale(ts),
            None => true,
        };
        self.resolve_status(&release, &cache.dismissed, stale, last_checked)
    }

    /// Record `version` as dismissed. The next [`Self::check_cached`]
    /// or [`Self::check_now`] call returning the same release renders
    /// as up-to-date. Persist failures log at debug and silently
    /// no-op; dismissing is best-effort.
    pub fn dismiss(&self, version: &Version) {
        let _guard = self.cache_lock.lock().ok();
        let mut cache = read_cache(&self.cache_path).unwrap_or_default();
        if !cache.dismissed.contains(version) {
            cache.dismissed.push(version.clone());
        }
        if let Err(e) = write_cache(&self.cache_path, &cache) {
            tracing::debug!(error = %e, "update dismiss: cache write failed");
        }
    }

    fn is_stale(&self, last_checked: DateTime<Utc>) -> bool {
        // Strictly greater-than. A check that landed exactly on the
        // boundary is "still fresh"; the next tick after that is
        // stale. Wall-clock comparisons are coarse but the cache file
        // never carries sub-second precision either.
        let now = Utc::now();
        let interval = chrono::Duration::hours(i64::from(self.config.check_interval_hours));
        now.signed_duration_since(last_checked) > interval
    }

    fn resolve_status(
        &self,
        release: &ReleaseInfo,
        dismissed: &[Version],
        stale: bool,
        last_checked: Option<DateTime<Utc>>,
    ) -> UpdateStatus {
        let dismissed_match = dismissed.iter().any(|v| v == &release.version);
        if release.version <= self.current || dismissed_match {
            return UpdateStatus::UpToDate {
                current: self.current.clone(),
            };
        }
        if stale {
            return UpdateStatus::Stale {
                last_checked: last_checked.unwrap_or_else(Utc::now),
                latest: release.clone(),
            };
        }
        UpdateStatus::Available {
            current: self.current.clone(),
            latest: release.clone(),
        }
    }
}

fn read_cache(path: &Path) -> Result<CacheFile, UpdateError> {
    let bytes = std::fs::read(path)?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn write_cache(path: &Path, cache: &CacheFile) -> Result<(), UpdateError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec_pretty(cache)?;
    std::fs::write(path, body)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex as StdMutex};
    use tempfile::TempDir;

    /// Mock client: returns either a canned body or a canned error.
    /// Records every URL it was called with so tests can assert the
    /// no-network path was actually hit.
    struct MockClient {
        body: Result<String, String>,
        calls: StdMutex<Vec<String>>,
    }

    impl MockClient {
        fn ok(body: &str) -> Arc<Self> {
            Arc::new(Self {
                body: Ok(body.into()),
                calls: StdMutex::new(Vec::new()),
            })
        }

        fn err(msg: &str) -> Arc<Self> {
            Arc::new(Self {
                body: Err(msg.into()),
                calls: StdMutex::new(Vec::new()),
            })
        }

        fn call_count(&self) -> usize {
            self.calls.lock().map(|v| v.len()).unwrap_or(0)
        }
    }

    impl HttpClient for Arc<MockClient> {
        fn get(&self, url: &str) -> Result<String, String> {
            if let Ok(mut v) = self.calls.lock() {
                v.push(url.into());
            }
            self.body.clone()
        }
    }

    fn release_json(tag: &str) -> String {
        format!(
            r#"{{
                "tag_name": "{tag}",
                "published_at": "2026-01-01T00:00:00Z",
                "html_url": "https://github.com/kryptic-sh/buffr/releases/tag/{tag}",
                "body": "changelog"
            }}"#
        )
    }

    fn checker_with(
        client: Arc<MockClient>,
        current: &str,
        cfg: UpdateConfig,
    ) -> (UpdateChecker, TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("update-cache.json");
        let cur = Version::parse(current).unwrap();
        let chk = UpdateChecker::with_client_and_current(cfg, path, Box::new(client), cur);
        (chk, tmp)
    }

    #[test]
    fn disabled_short_circuits_check_now() {
        let client = MockClient::ok(&release_json("v9.9.9"));
        let cfg = UpdateConfig {
            enabled: false,
            ..UpdateConfig::default()
        };
        let (chk, _tmp) = checker_with(client.clone(), "0.0.1", cfg);
        let status = chk.check_now();
        assert!(matches!(status, UpdateStatus::Disabled));
        assert_eq!(client.call_count(), 0);
    }

    #[test]
    fn disabled_short_circuits_check_cached() {
        let client = MockClient::ok(&release_json("v9.9.9"));
        let cfg = UpdateConfig {
            enabled: false,
            ..UpdateConfig::default()
        };
        let (chk, _tmp) = checker_with(client.clone(), "0.0.1", cfg);
        let status = chk.check_cached();
        assert!(matches!(status, UpdateStatus::Disabled));
        assert_eq!(client.call_count(), 0);
    }

    #[test]
    fn check_now_reports_available_for_newer_release() {
        let client = MockClient::ok(&release_json("v0.1.0"));
        let cfg = UpdateConfig::default();
        let (chk, _tmp) = checker_with(client, "0.0.4", cfg);
        match chk.check_now() {
            UpdateStatus::Available { current, latest } => {
                assert_eq!(current, Version::parse("0.0.4").unwrap());
                assert_eq!(latest.version, Version::parse("0.1.0").unwrap());
                assert_eq!(latest.tag, "v0.1.0");
            }
            other => panic!("expected Available, got {other:?}"),
        }
    }

    #[test]
    fn check_now_reports_up_to_date_when_equal() {
        let client = MockClient::ok(&release_json("v0.1.0"));
        let cfg = UpdateConfig::default();
        let (chk, _tmp) = checker_with(client, "0.1.0", cfg);
        match chk.check_now() {
            UpdateStatus::UpToDate { current } => {
                assert_eq!(current, Version::parse("0.1.0").unwrap());
            }
            other => panic!("expected UpToDate, got {other:?}"),
        }
    }

    #[test]
    fn semver_ordering_handles_rc() {
        // 0.1.0-rc.1 sorts BEFORE 0.1.0 by semver rules — rc/beta is
        // a pre-release so it's "less than" the stable.
        let client = MockClient::ok(&release_json("v0.1.0"));
        let cfg = UpdateConfig::default();
        let (chk, _tmp) = checker_with(client, "0.1.0-rc.1", cfg);
        match chk.check_now() {
            UpdateStatus::Available { latest, .. } => {
                assert_eq!(latest.version, Version::parse("0.1.0").unwrap());
            }
            other => panic!("expected Available, got {other:?}"),
        }
    }

    #[test]
    fn semver_compares_0_0_4_lt_0_1_0_lt_1_0_0() {
        let a = Version::parse("0.0.4").unwrap();
        let b = Version::parse("0.1.0").unwrap();
        let c = Version::parse("1.0.0").unwrap();
        assert!(a < b);
        assert!(b < c);
        let pre = Version::parse("0.1.0-rc.1").unwrap();
        assert!(pre < b);
    }

    #[test]
    fn check_cached_returns_stale_when_old() {
        let client = MockClient::ok(&release_json("v0.1.0"));
        let cfg = UpdateConfig {
            check_interval_hours: 24,
            ..UpdateConfig::default()
        };
        let (chk, _tmp) = checker_with(client, "0.0.1", cfg);
        // Run check_now once to populate cache.
        let _ = chk.check_now();
        // Mutate the cache to backdate the timestamp by 48 hours.
        let mut cache: CacheFile =
            serde_json::from_slice(&std::fs::read(&chk.cache_path).unwrap()).unwrap();
        cache.last_checked = Some(Utc::now() - chrono::Duration::hours(48));
        write_cache(&chk.cache_path, &cache).unwrap();

        match chk.check_cached() {
            UpdateStatus::Stale { latest, .. } => {
                assert_eq!(latest.version, Version::parse("0.1.0").unwrap());
            }
            other => panic!("expected Stale, got {other:?}"),
        }
    }

    #[test]
    fn check_cached_returns_available_when_fresh() {
        let client = MockClient::ok(&release_json("v0.1.0"));
        let cfg = UpdateConfig {
            check_interval_hours: 24,
            ..UpdateConfig::default()
        };
        let (chk, _tmp) = checker_with(client, "0.0.1", cfg);
        let _ = chk.check_now();
        match chk.check_cached() {
            UpdateStatus::Available { latest, .. } => {
                assert_eq!(latest.version, Version::parse("0.1.0").unwrap());
            }
            other => panic!("expected Available, got {other:?}"),
        }
    }

    #[test]
    fn network_error_is_returned_without_panic() {
        let client = MockClient::err("connection refused");
        let cfg = UpdateConfig::default();
        let (chk, _tmp) = checker_with(client, "0.0.1", cfg);
        match chk.check_now() {
            UpdateStatus::NetworkError(msg) => assert!(msg.contains("connection refused")),
            other => panic!("expected NetworkError, got {other:?}"),
        }
    }

    #[test]
    fn dismiss_filters_subsequent_lookups() {
        let client = MockClient::ok(&release_json("v0.1.0"));
        let cfg = UpdateConfig::default();
        let (chk, _tmp) = checker_with(client, "0.0.1", cfg);
        // Sanity: pre-dismiss it is Available.
        assert!(matches!(chk.check_now(), UpdateStatus::Available { .. }));
        chk.dismiss(&Version::parse("0.1.0").unwrap());
        match chk.check_cached() {
            UpdateStatus::UpToDate { current } => {
                assert_eq!(current, Version::parse("0.0.1").unwrap());
            }
            other => panic!("expected UpToDate after dismiss, got {other:?}"),
        }
    }

    #[test]
    fn no_cache_file_returns_up_to_date() {
        let client = MockClient::ok(&release_json("v0.1.0"));
        let cfg = UpdateConfig::default();
        let (chk, _tmp) = checker_with(client, "0.0.1", cfg);
        // Skip check_now — cache file does not exist.
        assert!(matches!(chk.check_cached(), UpdateStatus::UpToDate { .. }));
    }

    #[test]
    fn malformed_release_json_returns_network_error() {
        let client = MockClient::ok("{ not really json");
        let cfg = UpdateConfig::default();
        let (chk, _tmp) = checker_with(client, "0.0.1", cfg);
        assert!(matches!(chk.check_now(), UpdateStatus::NetworkError(_)));
    }

    #[test]
    fn non_semver_tag_returns_network_error() {
        let client = MockClient::ok(&release_json("nightly-banana"));
        let cfg = UpdateConfig::default();
        let (chk, _tmp) = checker_with(client, "0.0.1", cfg);
        match chk.check_now() {
            UpdateStatus::NetworkError(msg) => assert!(msg.contains("nightly-banana")),
            other => panic!("expected NetworkError, got {other:?}"),
        }
    }

    #[test]
    fn update_config_round_trip_toml() {
        let cfg = UpdateConfig::default();
        let s = toml::to_string(&cfg).unwrap();
        let back: UpdateConfig = toml::from_str(&s).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn update_config_unknown_field_rejected() {
        let toml = r#"
enabled = true
channel = "stable"
check_interval_hours = 24
github_repo = "kryptic-sh/buffr"
mystery = 42
"#;
        let err = toml::from_str::<UpdateConfig>(toml).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("mystery") || msg.contains("unknown"));
    }
}
