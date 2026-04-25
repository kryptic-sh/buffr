//! Disk loader: XDG path resolution + parse + span-aware error mapping.

use std::path::{Path, PathBuf};

use directories::ProjectDirs;

use crate::{Config, ConfigError, locate};

/// Where the loaded config came from. Useful for logging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSource {
    /// User config file at this path.
    File(PathBuf),
    /// No user file present; built-in defaults.
    Defaults,
}

/// Resolve the default `config.toml` path via `directories::ProjectDirs`.
///
/// - Linux: `$XDG_CONFIG_HOME/buffr/config.toml` (or `~/.config/buffr/...`).
/// - macOS: `~/Library/Application Support/buffr/config.toml`.
/// - Windows: `%APPDATA%\buffr\config.toml`.
///
/// Returns `None` if the platform doesn't expose a home dir at all
/// (effectively only sandboxed test environments).
pub fn default_config_path() -> Option<PathBuf> {
    let dirs = ProjectDirs::from("sh", "kryptic", "buffr")?;
    Some(dirs.config_dir().join("config.toml"))
}

/// Load + parse the user's `config.toml` from the default XDG path.
///
/// Returns `(Config::default(), Defaults)` if the file is absent.
pub fn load() -> Result<(Config, ConfigSource), ConfigError> {
    let Some(path) = default_config_path() else {
        return Ok((Config::default(), ConfigSource::Defaults));
    };
    if !path.exists() {
        return Ok((Config::default(), ConfigSource::Defaults));
    }
    load_from_path(&path)
}

/// Load + parse from an explicit path. Used by `--config <PATH>` and tests.
pub fn load_from_path(path: &Path) -> Result<(Config, ConfigSource), ConfigError> {
    let src = std::fs::read_to_string(path).map_err(|e| ConfigError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    let cfg: Config = toml::from_str(&src).map_err(|e| {
        let span = e.span().unwrap_or(0..0);
        let (line, col, snippet) = locate(&src, span.start);
        ConfigError::Parse {
            path: path.to_path_buf(),
            message: e.message().to_string(),
            line,
            col,
            snippet,
        }
    })?;
    Ok((cfg, ConfigSource::File(path.to_path_buf())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn load_from_tmpfile() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(
            f,
            r#"
[general]
homepage = "https://kryptic.sh"
leader = "\\"
"#
        )
        .unwrap();
        let (cfg, src) = load_from_path(f.path()).unwrap();
        assert_eq!(cfg.general.homepage, "https://kryptic.sh");
        match src {
            ConfigSource::File(p) => assert_eq!(p, f.path()),
            _ => panic!("expected File source"),
        }
    }

    #[test]
    fn load_missing_returns_defaults_via_load_fn() {
        let nonexistent = PathBuf::from("/nonexistent/buffr-config-test/foo.toml");
        let err = load_from_path(&nonexistent).unwrap_err();
        assert!(matches!(err, ConfigError::Io { .. }));
    }

    #[test]
    fn parse_error_reports_line_col() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "[general]").unwrap();
        writeln!(f, "this is not valid toml = =").unwrap();
        let err = load_from_path(f.path()).unwrap_err();
        match err {
            ConfigError::Parse { line, .. } => assert!(line >= 2),
            other => panic!("expected Parse, got {other:?}"),
        }
    }
}
