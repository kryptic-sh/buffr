# buffr-config

TOML config loader, validator, and hot-reload watcher for buffr.

[![CI](https://github.com/kryptic-sh/buffr/actions/workflows/ci.yml/badge.svg)](https://github.com/kryptic-sh/buffr/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](../../LICENSE)
[![Website](https://img.shields.io/badge/website-buffr.kryptic.sh-7ee787)](https://buffr.kryptic.sh)

Parses `config.toml` via XDG path resolution
(`directories::ProjectDirs::from("sh", "kryptic", "buffr")`). Returns
`(Config, ConfigSource)` so callers distinguish "user file loaded" from
"built-in defaults". Parse errors carry line / column spans from
`toml::de::Error::span()`. Hot reload via a `notify` debounced watcher that
re-parses and re-validates on filesystem events.

## Status

`0.0.1` — full schema: `[general]`, `[startup]`, `[search]`, `[theme]`,
`[privacy]`, `[downloads]`, `[hint]`, `[crash_reporter]`, `[updates]`,
`[accessibility]`, `[keymap.<mode>]`. Validation rejects bad leader char,
missing search engines, bad hint alphabet, and unparseable keybinding strings.

## Usage

```toml
# Cargo.toml (workspace path dep)
buffr-config = { path = "crates/buffr-config" }
```

```rust,no_run
use buffr_config::{load_and_validate, build_keymap, Config, ConfigSource};

// Load from the XDG default path or fall back to Config::default().
let (cfg, src) = load_and_validate(None)?;
println!("loaded from {:?}", src);

// Build the live keymap (defaults + user overrides).
let keymap = build_keymap(&cfg)?;

// Hot-reload watcher — calls `on_change` whenever config.toml changes.
let _watcher = buffr_config::watch(None, Box::new(move |new_cfg| {
    // swap keymap, apply theme changes, etc.
    let _ = new_cfg;
}))?;
```

## Config schema

Top-level sections (`deny_unknown_fields` — unknown keys are parse errors):

| Section            | Key fields                                                             |
| ------------------ | ---------------------------------------------------------------------- |
| `[general]`        | `homepage`, `leader` (single char, default `\`)                        |
| `[startup]`        | `restore_session`, `new_tab_url`                                       |
| `[search]`         | `default_engine`, `[search.engines.<name>] url = "…"`                  |
| `[theme]`          | `accent` (hex), `mode` (`auto`/`dark`/`light`), `high_contrast`        |
| `[privacy]`        | `enable_telemetry`, `clear_on_exit`, `skip_schemes`                    |
| `[downloads]`      | `default_dir`, `open_on_finish`, `ask_each_time`, `show_notifications` |
| `[hint]`           | `alphabet` (ASCII, ≥ 2 chars, no duplicates)                           |
| `[crash_reporter]` | `enabled`, `purge_after_days`                                          |
| `[updates]`        | `enabled`, `channel`, `check_interval_hours`, `github_repo`            |
| `[accessibility]`  | `force_renderer_accessibility`                                         |
| `[keymap.<mode>]`  | `"<chord>" = "<action>"` — overrides on top of defaults                |

`clear_on_exit` accepts: `"cookies"`, `"cache"`, `"history"`, `"bookmarks"`,
`"downloads"`, `"local_storage"`.

### Minimal example

```toml
[general]
homepage = "https://example.com"
leader = " "

[search]
default_engine = "duckduckgo"

[search.engines.duckduckgo]
url = "https://duckduckgo.com/?q={query}"

[keymap.normal]
"t" = "tab_new_right"
"gd" = "open_dev_tools"
```

## Key API

| Function / type               | Purpose                                                              |
| ----------------------------- | -------------------------------------------------------------------- |
| `load()`                      | Load from XDG default path or return `Config::default()`.            |
| `load_from_path(p)`           | Load from an explicit path.                                          |
| `load_and_validate(opt_path)` | Load + validate in one call.                                         |
| `validate(cfg)`               | Run all validation rules; return `ConfigError::Validate` on failure. |
| `build_keymap(cfg)`           | Apply `cfg.keymap` overrides on top of `Keymap::default_bindings`.   |
| `watch(opt_path, cb)`         | Spawn a debounced filesystem watcher; fires `cb` on change.          |
| `to_toml_string(cfg)`         | Serialize `Config` back to TOML (used by `--print-config`).          |
| `classify_input(s)`           | Categorise an omnibar string as URL, Host, or Search.                |
| `resolve_input(s, engines)`   | Resolve to a final URL (expands `{query}` template).                 |
| `resolve_default_dir(cfg)`    | Compute the effective download directory.                            |
| `Config`                      | Top-level config struct (all sections).                              |
| `ConfigError`                 | `Io`, `Parse` (with line/col/snippet), `Validate`.                   |
| `ConfigSource`                | `DefaultPath(PathBuf)`, `ExplicitPath(PathBuf)`, `Defaults`.         |

## License

MIT. See [LICENSE](../../LICENSE).
