//! Config loading and parsing.
//!
//! Phase 4 lands a richer schema:
//!
//! - `[general]` — homepage, leader char.
//! - `[startup]` — session restore, new-tab URL.
//! - `[search]` — default engine + per-engine URL templates.
//! - `[theme]` — accent + dark/light mode.
//! - `[privacy]` — telemetry / clear-on-exit.
//! - `[keymap.<mode>]` — `"j" = "scroll_down"` style entries that
//!   parse into [`buffr_modal::PageAction`].
//!
//! XDG path resolution via `directories::ProjectDirs::from("sh",
//! "kryptic", "buffr")`. Loader returns `(Config, ConfigSource)` so
//! callers can distinguish "user file loaded" from "defaults". Errors
//! carry line/column spans extracted from `toml::de::Error::span()`.
//!
//! Hot reload via [`watch`] which spawns a `notify` debounced watcher
//! and re-parses + re-validates on filesystem events.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use directories::UserDirs;
use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

pub use buffr_modal::{PageAction, PageMode};

mod keybinding;
mod loader;
pub mod search;
mod watcher;

pub use keybinding::{KeyBinding, KeyBindingError, parse_action};
pub use loader::{ConfigSource, default_config_path, load, load_from_path};
pub use search::{InputKind, classify_input, resolve_input};
pub use watcher::{ConfigWatcher, watch};

/// Top-level config.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub general: General,
    pub startup: Startup,
    pub search: Search,
    pub theme: Theme,
    pub privacy: Privacy,
    pub downloads: DownloadsConfig,
    pub hint: HintConfig,
    pub crash_reporter: CrashReporterConfig,
    pub updates: UpdateConfig,
    pub accessibility: AccessibilityConfig,
    #[serde(deserialize_with = "deserialize_keymap")]
    pub keymap: HashMap<PageMode, HashMap<String, KeyBinding>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct General {
    pub homepage: String,
    pub leader: String,
}

impl Default for General {
    fn default() -> Self {
        Self {
            homepage: "https://example.com".into(),
            leader: "\\".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Startup {
    pub restore_session: bool,
    pub new_tab_url: String,
}

impl Default for Startup {
    fn default() -> Self {
        Self {
            restore_session: false,
            new_tab_url: "about:blank".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Search {
    pub default_engine: String,
    pub engines: HashMap<String, SearchEngine>,
}

impl Default for Search {
    fn default() -> Self {
        let mut engines = HashMap::new();
        engines.insert(
            "duckduckgo".into(),
            SearchEngine {
                url: "https://duckduckgo.com/?q={query}".into(),
            },
        );
        engines.insert(
            "google".into(),
            SearchEngine {
                url: "https://www.google.com/search?q={query}".into(),
            },
        );
        Self {
            default_engine: "duckduckgo".into(),
            engines,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchEngine {
    pub url: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    #[default]
    Auto,
    Dark,
    Light,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Theme {
    pub accent: String,
    pub mode: ThemeMode,
    /// Phase 6 accessibility: when `true`, the chrome (statusline,
    /// tab strip, input bar, prompt) renders with a high-contrast
    /// palette instead of the accent-tinted default. See
    /// `docs/accessibility.md` for the colour values.
    pub high_contrast: bool,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            accent: "#7aa2f7".into(),
            mode: ThemeMode::Auto,
            high_contrast: false,
        }
    }
}

/// `[updates]` section — Phase 6 update channel.
///
/// Off-by-default would make us silently fall behind on security fixes,
/// so the default is `enabled = true` with a 24 h interval. This
/// performs **one** GET per `check_interval_hours` against the GitHub
/// releases API; no PII is sent. To opt out fully set
/// `enabled = false` — that path makes **zero** network calls. See
/// `docs/privacy.md`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UpdateConfig {
    pub enabled: bool,
    /// Release channel. `"stable"` only today; `"nightly"` reserved.
    pub channel: String,
    pub check_interval_hours: u32,
    /// `owner/repo` slug. Forks point this at their own repo.
    pub github_repo: String,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            channel: "stable".into(),
            check_interval_hours: 24,
            github_repo: "kryptic-sh/buffr".into(),
        }
    }
}

/// `[accessibility]` section — Phase 6 a11y pass.
///
/// Web content goes through CEF's renderer accessibility tree (which
/// platform screen readers read directly). Native chrome (statusline,
/// tab strip, prompts) is keyboard-first; a screen-reader bridge is
/// post-1.0 because it requires platform-specific bindings (AT-SPI on
/// Linux, NSAccessibility on macOS, MSAA on Windows). See
/// `docs/accessibility.md`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AccessibilityConfig {
    /// Enable CEF's renderer accessibility tree by passing the
    /// `--force-renderer-accessibility` switch through
    /// `App::on_before_command_line_processing`. Default `false` —
    /// the tree is non-trivial overhead for users without an AT.
    pub force_renderer_accessibility: bool,
}

/// Categories of locally-stored data the shutdown hook can wipe when
/// listed in `[privacy] clear_on_exit`. Variants map onto independent
/// teardown paths in `apps/buffr/src/main.rs::run_clear_on_exit`. Adding
/// a variant here is the only place a new clearable category needs to
/// register — the shutdown hook matches exhaustively.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ClearableData {
    /// Cookies, via the CEF global `CookieManager::delete_cookies`.
    Cookies,
    /// CEF cache directory (`<root_cache_path>/Cache`).
    Cache,
    /// `buffr-history` SQLite store — every visit row.
    History,
    /// `buffr-bookmarks` SQLite store — every bookmark + tag. Listed
    /// here is destructive and only honored when explicit.
    Bookmarks,
    /// `buffr-downloads` SQLite store — every row regardless of status.
    Downloads,
    /// CEF localStorage / IndexedDB tree
    /// (`<root_cache_path>/Local Storage`).
    LocalStorage,
}

/// Default schemes suppressed by [`buffr_history`].
pub const DEFAULT_SKIP_SCHEMES: &[&str] = &["about", "cef", "chrome", "data", "file"];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Privacy {
    pub enable_telemetry: bool,
    pub clear_on_exit: Vec<ClearableData>,
    /// URL schemes whose visits are never recorded in history. Matching
    /// is case-insensitive on the scheme component. The default list
    /// (`about`, `cef`, `chrome`, `data`, `file`) covers internal
    /// browser pages and privacy-sensitive local URLs. Add entries to
    /// suppress additional schemes (e.g. `"javascript"`, `"blob"`);
    /// remove entries to record them (unusual but supported).
    pub skip_schemes: Vec<String>,
}

impl Default for Privacy {
    fn default() -> Self {
        Self {
            enable_telemetry: false,
            clear_on_exit: Vec::new(),
            skip_schemes: DEFAULT_SKIP_SCHEMES.iter().map(|s| s.to_string()).collect(),
        }
    }
}

/// `[crash_reporter]` section — Phase 6 opt-in panic reporter.
///
/// Off by default. Reports are written to
/// `<data>/crashes/<timestamp>.json` for the user to inspect or submit
/// by hand; nothing is uploaded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CrashReporterConfig {
    /// Master switch. `false` (the default) leaves the default panic
    /// hook in place; `true` installs the buffr panic hook on startup.
    pub enabled: bool,
    /// Auto-purge cutoff. Reports older than this many days are
    /// removed by `--purge-crashes`. Default `30`.
    pub purge_after_days: u32,
}

impl Default for CrashReporterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            purge_after_days: 30,
        }
    }
}

/// `[hint]` section — Phase 3 follow-by-letter labels.
///
/// `alphabet` is the ordered character list used to mint hint labels.
/// Default mirrors Vimium's home-row plus upper-row; users can prefer
/// `"abcdefghijklmnopqrstuvwxyz"` for full-keyboard reachability or
/// trim to a smaller set if they only use the home row.
///
/// Validation:
///
/// - non-empty
/// - no duplicates
/// - ASCII only (the injected JS asserts ASCII so we keep the
///   alphabet ASCII end-to-end)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HintConfig {
    pub alphabet: String,
}

impl Default for HintConfig {
    fn default() -> Self {
        Self {
            // Mirrors `buffr_core::DEFAULT_HINT_ALPHABET`. We can't
            // import that as a const here (would create a config →
            // core dep cycle); duplicate the literal and keep them in
            // sync via the `default_hint_alphabet_matches_core` test in
            // `buffr-core/src/hint.rs`.
            alphabet: "asdfghjkl;weruio".into(),
        }
    }
}

/// `[downloads]` section. Phase 5: governs where files land, whether
/// buffr opens them on completion, and (post-Phase 3) whether the
/// chrome surfaces a save-as dialog.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DownloadsConfig {
    /// User-specified default directory. `None` (the default) means
    /// "resolve at runtime via [`resolve_default_dir`]".
    pub default_dir: Option<PathBuf>,
    /// Open the downloaded file once it finishes via the platform's
    /// open command (`xdg-open` / `open` / `start`). Default `false`.
    pub open_on_finish: bool,
    /// Show a save-as dialog for every download. Default `false` —
    /// downloads silently land in `default_dir`. The actual dialog UI
    /// is Phase 3 chrome work; setting this `true` before then is
    /// effectively a no-op (CEF's `BeforeDownloadCallback::cont`
    /// receives `show_dialog = 1` but no native dialog is wired yet).
    pub ask_each_time: bool,
}

/// Resolve the effective default download directory.
///
/// Resolution order:
///
/// 1. `cfg.downloads.default_dir` if `Some` and absolute. Tilde and
///    other shell expansions are *not* applied — TOML carries no
///    shell.
/// 2. `directories::UserDirs::download_dir()` — the XDG
///    `XDG_DOWNLOAD_DIR` on Linux, `~/Downloads` on macOS,
///    `%USERPROFILE%\Downloads` on Windows.
/// 3. `$HOME/Downloads` (Unix) / `%USERPROFILE%\Downloads` (Windows)
///    as a last-resort fallback.
/// 4. Current working directory if even that's unavailable — we never
///    return `None` so callers don't have to handle a missing default.
pub fn resolve_default_dir(cfg: &DownloadsConfig) -> PathBuf {
    if let Some(p) = cfg.default_dir.as_ref() {
        return p.clone();
    }
    if let Some(dirs) = UserDirs::new()
        && let Some(d) = dirs.download_dir()
    {
        return d.to_path_buf();
    }
    // `$HOME/Downloads` fallback. `home_dir()` is deprecated on stable
    // for cross-platform reasons but we only need a best-effort
    // fallback — if it's wrong the user can set `default_dir`.
    #[allow(deprecated)]
    if let Some(home) = std::env::home_dir() {
        return home.join("Downloads");
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Errors the loader / validator can produce.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("io error reading {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("error: {message}\n  at {}:{line}:{col}\n   | {snippet}", path.display())]
    Parse {
        path: PathBuf,
        message: String,
        line: usize,
        col: usize,
        snippet: String,
    },
    #[error("config validation failed: {message}{}", location.as_ref().map(|l| format!(" ({l})")).unwrap_or_default())]
    Validate {
        message: String,
        location: Option<String>,
    },
}

/// Build a snapshot of every line/column for byte offsets in `src`.
/// Toml's `Error::span()` returns a byte range; we map the start to a
/// `(line, col, line_text)` triple.
pub(crate) fn locate(src: &str, byte_offset: usize) -> (usize, usize, String) {
    let clamped = byte_offset.min(src.len());
    let mut line_start = 0usize;
    let mut line_no = 1usize;
    for (i, ch) in src.char_indices() {
        if i >= clamped {
            break;
        }
        if ch == '\n' {
            line_no += 1;
            line_start = i + ch.len_utf8();
        }
    }
    let line_end = src[line_start..]
        .find('\n')
        .map(|n| line_start + n)
        .unwrap_or(src.len());
    let line_text = src[line_start..line_end].to_string();
    let col = clamped.saturating_sub(line_start) + 1;
    (line_no, col, line_text)
}

/// `[keymap]` deserializes as `HashMap<PageMode, HashMap<String, KeyBinding>>`,
/// but we want unknown modes to error out rather than silently slip past.
/// The standard `HashMap<PageMode, _>` deserializer surfaces unknown keys
/// as serde errors automatically (since `PageMode` is `deny_unknown_fields`
/// over its rename_all snake_case variants). The wrapper exists so we can
/// later add a normalization pass without changing the public type.
fn deserialize_keymap<'de, D>(
    deserializer: D,
) -> Result<HashMap<PageMode, HashMap<String, KeyBinding>>, D::Error>
where
    D: Deserializer<'de>,
{
    HashMap::<PageMode, HashMap<String, KeyBinding>>::deserialize(deserializer)
}

/// Validate a parsed `Config`. Called after `toml::from_str` succeeds.
///
/// Checks:
///
/// - `general.leader` is exactly one character.
/// - `search.default_engine` references a defined engine.
/// - Every keymap binding string parses via `buffr_modal::parse_keys`.
pub fn validate(cfg: &Config) -> Result<(), ConfigError> {
    let leader_chars: Vec<char> = cfg.general.leader.chars().collect();
    if leader_chars.len() != 1 {
        return Err(ConfigError::Validate {
            message: format!(
                "general.leader must be exactly one character (got {:?})",
                cfg.general.leader
            ),
            location: Some("general.leader".into()),
        });
    }

    // Hint alphabet validation. Surface any of the three failure modes
    // (empty, duplicate, non-ASCII) as a [`ConfigError::Validate`] so
    // the user gets a friendly toml-aware message instead of a panic
    // deep inside `BrowserHost::new`.
    {
        let alpha = &cfg.hint.alphabet;
        if alpha.is_empty() {
            return Err(ConfigError::Validate {
                message: "hint.alphabet must be non-empty".into(),
                location: Some("hint.alphabet".into()),
            });
        }
        if !alpha.is_ascii() {
            return Err(ConfigError::Validate {
                message: format!("hint.alphabet must be ASCII (got {alpha:?})"),
                location: Some("hint.alphabet".into()),
            });
        }
        let mut seen = std::collections::HashSet::new();
        for c in alpha.chars() {
            if !seen.insert(c) {
                return Err(ConfigError::Validate {
                    message: format!("hint.alphabet contains duplicate character {c:?}"),
                    location: Some("hint.alphabet".into()),
                });
            }
        }
        if alpha.chars().count() < 2 {
            return Err(ConfigError::Validate {
                message: "hint.alphabet must contain at least 2 characters".into(),
                location: Some("hint.alphabet".into()),
            });
        }
    }

    // -- updates ------------------------------------------------------
    // Validate channel + repo shape so a typo doesn't surface as a 404
    // GET burned each launch. Keep accepted channels permissive so
    // `nightly` parses now even though the actual nightly tag stream
    // isn't published yet.
    {
        let ch = cfg.updates.channel.as_str();
        if ch != "stable" && ch != "nightly" {
            return Err(ConfigError::Validate {
                message: format!("updates.channel must be \"stable\" or \"nightly\" (got {ch:?})"),
                location: Some("updates.channel".into()),
            });
        }
        let repo = cfg.updates.github_repo.as_str();
        // GitHub repos are `owner/repo` — exactly one slash, no empty
        // segments. Anything else routes to a 404 we'd rather catch
        // here than at HTTP time.
        let slash_count = repo.matches('/').count();
        if slash_count != 1 || repo.starts_with('/') || repo.ends_with('/') {
            return Err(ConfigError::Validate {
                message: format!(
                    "updates.github_repo must be of the form \"owner/repo\" (got {repo:?})"
                ),
                location: Some("updates.github_repo".into()),
            });
        }
        if cfg.updates.check_interval_hours == 0 {
            return Err(ConfigError::Validate {
                message: "updates.check_interval_hours must be > 0".into(),
                location: Some("updates.check_interval_hours".into()),
            });
        }
    }

    if !cfg.search.engines.contains_key(&cfg.search.default_engine) {
        return Err(ConfigError::Validate {
            message: format!(
                "search.default_engine = {:?} but no [search.engines.{}] block defined",
                cfg.search.default_engine, cfg.search.default_engine
            ),
            location: Some("search.default_engine".into()),
        });
    }

    for (mode, bindings) in &cfg.keymap {
        for keys in bindings.keys() {
            if let Err(e) = buffr_modal::parse_keys(keys) {
                return Err(ConfigError::Validate {
                    message: format!(
                        "keymap.{} binding {:?} failed to parse: {}",
                        mode_name(*mode),
                        keys,
                        e
                    ),
                    location: Some(format!("keymap.{}.{:?}", mode_name(*mode), keys)),
                });
            }
        }
    }

    Ok(())
}

/// Apply the user's [`Config`] keymap on top of [`buffr_modal::Keymap::default_bindings`].
///
/// Phase 4 strategy: take the built-in defaults for the configured leader,
/// then for every `(mode, keys, action)` in the user config rebind on top.
/// `buffr_modal::Keymap::bind` overwrites existing entries, so the user's
/// table is effectively a per-binding override.
pub fn build_keymap(cfg: &Config) -> Result<buffr_modal::Keymap, ConfigError> {
    let leader = cfg
        .general
        .leader
        .chars()
        .next()
        .ok_or_else(|| ConfigError::Validate {
            message: "general.leader is empty".into(),
            location: Some("general.leader".into()),
        })?;
    let mut km = buffr_modal::Keymap::default_bindings(leader);
    for (mode, bindings) in &cfg.keymap {
        for (keys, binding) in bindings {
            km.bind(*mode, keys, binding.action.clone())
                .map_err(|e| ConfigError::Validate {
                    message: format!(
                        "failed to bind keymap.{}.{:?}: {}",
                        mode_name(*mode),
                        keys,
                        e
                    ),
                    location: Some(format!("keymap.{}.{:?}", mode_name(*mode), keys)),
                })?;
        }
    }
    Ok(km)
}

fn mode_name(mode: PageMode) -> &'static str {
    match mode {
        PageMode::Normal => "normal",
        PageMode::Visual => "visual",
        PageMode::Command => "command",
        PageMode::Hint => "hint",
        PageMode::Pending => "pending",
        PageMode::Edit => "edit",
    }
}

/// Serialize `cfg` back to TOML. Used by `--print-config`.
pub fn to_toml_string(cfg: &Config) -> Result<String, toml::ser::Error> {
    toml::to_string_pretty(cfg)
}

/// Convenience: load + validate + return path source.
pub fn load_and_validate(path: Option<&Path>) -> Result<(Config, ConfigSource), ConfigError> {
    let (cfg, src) = match path {
        Some(p) => load_from_path(p)?,
        None => load()?,
    };
    validate(&cfg)?;
    Ok((cfg, src))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_round_trip() {
        let cfg = Config::default();
        let s = to_toml_string(&cfg).unwrap();
        let parsed: Config = toml::from_str(&s).unwrap();
        assert_eq!(parsed, cfg);
    }

    #[test]
    fn parse_error_carries_location() {
        let bad = "[general]\nhomepage = 12 = oops\n";
        let err = toml::from_str::<Config>(bad).unwrap_err();
        let span = err.span().expect("span available");
        let (line, _col, text) = locate(bad, span.start);
        assert_eq!(line, 2);
        assert!(text.contains("homepage"));
    }

    #[test]
    fn validate_leader_single_char_ok() {
        let mut cfg = Config::default();
        cfg.general.leader = "\\".into();
        validate(&cfg).unwrap();
    }

    #[test]
    fn validate_leader_empty_errors() {
        let mut cfg = Config::default();
        cfg.general.leader = String::new();
        let err = validate(&cfg).unwrap_err();
        assert!(matches!(err, ConfigError::Validate { .. }));
    }

    #[test]
    fn validate_leader_multi_char_errors() {
        let mut cfg = Config::default();
        cfg.general.leader = "abc".into();
        let err = validate(&cfg).unwrap_err();
        assert!(matches!(err, ConfigError::Validate { .. }));
    }

    #[test]
    fn validate_default_engine_must_exist() {
        let mut cfg = Config::default();
        cfg.search.default_engine = "ddg".into();
        let err = validate(&cfg).unwrap_err();
        match err {
            ConfigError::Validate { message, .. } => assert!(message.contains("ddg")),
            _ => panic!("expected Validate error"),
        }
    }

    #[test]
    fn keymap_binding_parses_unit_variant() {
        let toml = r#"
[keymap.normal]
"j" = "scroll_down"
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        let bindings = cfg.keymap.get(&PageMode::Normal).unwrap();
        assert_eq!(bindings.get("j").unwrap().action, PageAction::ScrollDown(1));
    }

    #[test]
    fn keymap_binding_parses_count_variant() {
        let toml = r#"
[keymap.normal]
"5j" = "scroll_down(5)"
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        let bindings = cfg.keymap.get(&PageMode::Normal).unwrap();
        assert_eq!(
            bindings.get("5j").unwrap().action,
            PageAction::ScrollDown(5)
        );
    }

    #[test]
    fn keymap_binding_malformed_errors() {
        let toml = r#"
[keymap.normal]
"j" = "fly"
"#;
        let err = toml::from_str::<Config>(toml).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("fly") || msg.contains("unknown"));
    }

    #[test]
    fn keymap_find_with_forward_arg() {
        let toml = r#"
[keymap.normal]
"/" = "find(forward = true)"
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        let bindings = cfg.keymap.get(&PageMode::Normal).unwrap();
        assert_eq!(
            bindings.get("/").unwrap().action,
            PageAction::Find { forward: true }
        );
    }

    #[test]
    fn downloads_section_defaults() {
        let cfg = Config::default();
        assert!(cfg.downloads.default_dir.is_none());
        assert!(!cfg.downloads.open_on_finish);
        assert!(!cfg.downloads.ask_each_time);
    }

    #[test]
    fn downloads_section_parses_from_toml() {
        let toml = r#"
[downloads]
default_dir = "/tmp/buffr-dl"
open_on_finish = true
ask_each_time = false
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(
            cfg.downloads.default_dir.as_deref(),
            Some(std::path::Path::new("/tmp/buffr-dl"))
        );
        assert!(cfg.downloads.open_on_finish);
        assert!(!cfg.downloads.ask_each_time);
    }

    #[test]
    fn downloads_unknown_field_rejected() {
        let toml = r#"
[downloads]
weird = "nope"
"#;
        let err = toml::from_str::<Config>(toml).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("weird") || msg.contains("unknown"));
    }

    #[test]
    fn resolve_default_dir_uses_explicit_value() {
        let cfg = DownloadsConfig {
            default_dir: Some(PathBuf::from("/tmp/explicit")),
            ..Default::default()
        };
        assert_eq!(resolve_default_dir(&cfg), PathBuf::from("/tmp/explicit"));
    }

    #[test]
    fn resolve_default_dir_falls_back_to_user_dirs() {
        let cfg = DownloadsConfig::default();
        // We can't predict the exact path on every test host but the
        // call must produce *some* path — never panic, never empty.
        let p = resolve_default_dir(&cfg);
        assert!(!p.as_os_str().is_empty());
    }

    #[test]
    fn privacy_default_skip_schemes_contains_canonical_list() {
        let p = Privacy::default();
        for scheme in DEFAULT_SKIP_SCHEMES {
            assert!(
                p.skip_schemes.iter().any(|s| s == scheme),
                "default skip_schemes missing {scheme:?}"
            );
        }
        assert_eq!(p.skip_schemes.len(), DEFAULT_SKIP_SCHEMES.len());
    }

    #[test]
    fn privacy_skip_schemes_parses_from_toml() {
        let toml = r#"
[privacy]
skip_schemes = ["javascript", "blob"]
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.privacy.skip_schemes, vec!["javascript", "blob"]);
    }

    #[test]
    fn privacy_clear_on_exit_parses_known_variants() {
        let toml = r#"
[privacy]
enable_telemetry = false
clear_on_exit = ["cookies", "cache", "history", "bookmarks", "downloads", "local_storage"]
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(
            cfg.privacy.clear_on_exit,
            vec![
                ClearableData::Cookies,
                ClearableData::Cache,
                ClearableData::History,
                ClearableData::Bookmarks,
                ClearableData::Downloads,
                ClearableData::LocalStorage,
            ]
        );
    }

    #[test]
    fn privacy_clear_on_exit_unknown_variant_rejected() {
        let toml = r#"
[privacy]
clear_on_exit = ["everything"]
"#;
        let err = toml::from_str::<Config>(toml).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("everything") || msg.contains("unknown"));
    }

    #[test]
    fn hint_section_default() {
        let cfg = Config::default();
        assert_eq!(cfg.hint.alphabet, "asdfghjkl;weruio");
    }

    #[test]
    fn hint_section_parses_custom_alphabet() {
        let toml = r#"
[hint]
alphabet = "abcdef"
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.hint.alphabet, "abcdef");
        validate(&cfg).unwrap();
    }

    #[test]
    fn hint_alphabet_empty_rejected() {
        let mut cfg = Config::default();
        cfg.hint.alphabet = String::new();
        let err = validate(&cfg).unwrap_err();
        assert!(matches!(err, ConfigError::Validate { .. }));
    }

    #[test]
    fn hint_alphabet_duplicate_rejected() {
        let mut cfg = Config::default();
        cfg.hint.alphabet = "abca".into();
        let err = validate(&cfg).unwrap_err();
        match err {
            ConfigError::Validate { message, .. } => assert!(message.contains("duplicate")),
            _ => panic!("expected Validate error"),
        }
    }

    #[test]
    fn hint_alphabet_non_ascii_rejected() {
        let mut cfg = Config::default();
        cfg.hint.alphabet = "αβγ".into();
        let err = validate(&cfg).unwrap_err();
        match err {
            ConfigError::Validate { message, .. } => assert!(message.contains("ASCII")),
            _ => panic!("expected Validate error"),
        }
    }

    #[test]
    fn hint_alphabet_single_char_rejected() {
        let mut cfg = Config::default();
        cfg.hint.alphabet = "a".into();
        let err = validate(&cfg).unwrap_err();
        assert!(matches!(err, ConfigError::Validate { .. }));
    }

    #[test]
    fn hint_unknown_field_rejected() {
        let toml = r#"
[hint]
weird = "x"
"#;
        let err = toml::from_str::<Config>(toml).unwrap_err();
        assert!(err.to_string().contains("weird") || err.to_string().contains("unknown"));
    }

    #[test]
    fn crash_reporter_section_default() {
        let cfg = Config::default();
        assert!(!cfg.crash_reporter.enabled);
        assert_eq!(cfg.crash_reporter.purge_after_days, 30);
    }

    #[test]
    fn crash_reporter_section_parses_from_toml() {
        let toml = r#"
[crash_reporter]
enabled = true
purge_after_days = 7
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert!(cfg.crash_reporter.enabled);
        assert_eq!(cfg.crash_reporter.purge_after_days, 7);
    }

    #[test]
    fn crash_reporter_unknown_field_rejected() {
        let toml = r#"
[crash_reporter]
weird = "x"
"#;
        let err = toml::from_str::<Config>(toml).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("weird") || msg.contains("unknown"));
    }

    #[test]
    fn full_config_roundtrip_parses() {
        let toml = r##"
[general]
homepage = "https://example.com"
leader = "\\"

[startup]
restore_session = false
new_tab_url = "about:blank"

[search]
default_engine = "duckduckgo"

[search.engines.duckduckgo]
url = "https://duckduckgo.com/?q={query}"

[theme]
accent = "#7aa2f7"
mode = "auto"

[privacy]
enable_telemetry = false
clear_on_exit = []
skip_schemes = ["about", "cef", "chrome", "data", "file"]

[downloads]
open_on_finish = false
ask_each_time = false

[keymap.normal]
"j" = "scroll_down"
"k" = "scroll_up"
"gg" = "scroll_top"
"##;
        let cfg: Config = toml::from_str(toml).unwrap();
        validate(&cfg).unwrap();
        assert_eq!(cfg.general.homepage, "https://example.com");
    }
}
