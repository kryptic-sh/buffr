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
mod watcher;

pub use keybinding::{KeyBinding, KeyBindingError, parse_action};
pub use loader::{ConfigSource, default_config_path, load, load_from_path};
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
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            accent: "#7aa2f7".into(),
            mode: ThemeMode::Auto,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Privacy {
    pub enable_telemetry: bool,
    pub clear_on_exit: Vec<String>,
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
