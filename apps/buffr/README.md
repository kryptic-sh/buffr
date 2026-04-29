# buffr

Vim-inspired browser. Native, GPU-accelerated. Rust + CEF.

[![CI](https://github.com/kryptic-sh/buffr/actions/workflows/ci.yml/badge.svg)](https://github.com/kryptic-sh/buffr/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](../../LICENSE)
[![Website](https://img.shields.io/badge/website-buffr.kryptic.sh-7ee787)](https://buffr.kryptic.sh)

Main browser binary. Single winit window; CEF renders web content via off-screen
rendering (OSR) on Linux and macOS; Windows uses native windowed embedding.
Modal keybindings driven by [`buffr-modal`](../../crates/buffr-modal).

## Status

`0.0.1` — multi-tab browsing; OAuth / `window.open` popups in their own native
window; vim modal engine wired for page-mode dispatch and insert-mode text
editing; SQLite data layers (history, bookmarks, downloads, permissions, zoom)
wired and persisted.

## Install

No pre-built binaries yet. Build from source:

```bash
# Vendor the CEF binary distribution (~500 MB extracted).
cargo xtask fetch-cef

cargo build --release -p buffr
```

See [`docs/dev.md`](../../docs/dev.md) for full prerequisites.

## Usage

```bash
buffr                          # open homepage
buffr https://example.com      # navigate immediately
buffr --config /path/to/config.toml  # explicit config path
buffr --print-config           # dump compiled config as TOML
buffr --private                # private-window profile (in-memory DB)
```

### Utility flags (no CEF init)

```bash
buffr --list-permissions       # print every stored permission decision
buffr --clear-permissions      # wipe the permissions store
buffr --forget-origin https://example.com  # remove one origin's permissions
buffr --list-zoom              # print every (domain, zoom) pair
buffr --clear-zoom             # wipe the zoom store
buffr --check-for-updates      # query GitHub releases and print update status
buffr --audit-keymap           # print every default keybinding in CSV form
```

<!-- Screenshots placeholder -->
<!-- TODO: add screenshots once the UI stabilises -->

## What works

- **Multi-tab**: tab strip at the top; tab open / close / reorder; pinned tabs;
  reopen closed tab; paste-URL-as-tab.
- **Vim modal modes**: Normal / Insert / Visual / Command / Hint with statusline
  mode indicator.
- **Omnibar**: URL + search unified input; DuckDuckGo default; configurable
  engines.
- **Hint mode**: follow-by-letter (`f` foreground, `F` background tab).
- **Find-in-page**: `/` forward, `?` backward, `n` / `N` next / prev.
- **History back / forward** — `J` / `K` and `<C-o>` / `<C-i>`.
- **Zoom**: `+` / `-` / `0` per-domain, persisted across sessions.
- **OAuth popups** (`window.open` with features / `NEW_POPUP` disposition) open
  in their own native window. `target="_blank"` links open as new tabs.
- **Data layers**: history, bookmarks, downloads, permissions, zoom — all
  SQLite, WAL mode.
- **Private mode** (`--private`): all data layers use in-memory DBs; no state
  persists.
- **Config hot-reload**: editing `config.toml` reloads the keymap live without
  restart.

## What's deferred

- Session restore (`startup.restore_session = true` parses but restore glue is
  TODO).
- Splits / multiple windows.
- Extensions.
- LSP / tree-sitter page annotations.

## Keybindings (Normal mode)

| Key(s)                | Action                                  |
| --------------------- | --------------------------------------- |
| `j` / `k` / `h` / `l` | Scroll down / up / left / right         |
| `<C-d>` / `<C-u>`     | Half-page down / up                     |
| `<C-f>` / `<C-b>`     | Full-page down / up                     |
| `gg` / `G`            | Scroll to top / bottom                  |
| `H` / `L`             | Prev / next tab                         |
| `gt` / `gT`           | Next / prev tab                         |
| `d` / `<C-w>`         | Close tab                               |
| `o` / `O`             | Open tab right / left of active         |
| `<C-t>`               | Open tab right                          |
| `u` / `<C-S-t>`       | Reopen closed tab                       |
| `<leader>p`           | Pin / unpin tab                         |
| `p` / `P`             | Paste clipboard URL (after/before)      |
| `<C-S-h>` / `<C-S-l>` | Move tab left / right                   |
| `J` / `K`             | History back / forward                  |
| `<C-o>` / `<C-i>`     | History back / forward                  |
| `r` / `R` / `<C-r>`   | Reload / hard-reload                    |
| `e` / `<C-l>`         | Open omnibar                            |
| `:` / `;`             | Open command line                       |
| `f` / `F`             | Hint mode (foreground / background tab) |
| `/` / `?`             | Find forward / backward                 |
| `n` / `N`             | Find next / prev                        |
| `y`                   | Yank page URL                           |
| `+` / `=`             | Zoom in                                 |
| `-` / `_`             | Zoom out                                |
| `0` / `)`             | Zoom reset                              |
| `i` / `gi`            | Focus first text input (Insert mode)    |
| `<Esc>`               | Exit Insert mode                        |
| `<F12>` / `<C-S-i>`   | DevTools                                |

Default leader is `\` (backslash). Override in `[general] leader`.

## Config

Config file location (platform defaults):

| Platform | Path                                              |
| -------- | ------------------------------------------------- |
| Linux    | `~/.config/buffr/config.toml`                     |
| macOS    | `~/Library/Application Support/buffr/config.toml` |
| Windows  | `%APPDATA%\buffr\config.toml`                     |

Override with `--config /path/to/config.toml`. See
[`config.example.toml`](../../config.example.toml) for the annotated full
schema.

### Minimal example

```toml
[general]
homepage = "https://example.com"
leader = "\\"

[search]
default_engine = "duckduckgo"

[search.engines.duckduckgo]
url = "https://duckduckgo.com/?q={query}"

[keymap.normal]
"t" = "tab_new_right"
```

## Storage

All data written under the platform data dir for `sh.kryptic.buffr`:

| Platform | Data dir                               |
| -------- | -------------------------------------- |
| Linux    | `~/.local/share/buffr/`                |
| macOS    | `~/Library/Application Support/buffr/` |
| Windows  | `%APPDATA%\buffr\`                     |

Files: `history.sqlite`, `bookmarks.sqlite`, `downloads.sqlite`,
`permissions.sqlite`, `zoom.sqlite`, `usage-counters.json`, `crashes/`.

## Related crates

- [`buffr-core`](../../crates/buffr-core) — CEF integration, browser host
- [`buffr-modal`](../../crates/buffr-modal) — vim modal engine, keymap
- [`buffr-ui`](../../crates/buffr-ui) — chrome rendering
- [`buffr-config`](../../crates/buffr-config) — TOML config
- [`buffr-history`](../../crates/buffr-history) — browsing history
- [`buffr-bookmarks`](../../crates/buffr-bookmarks) — bookmarks
- [`buffr-downloads`](../../crates/buffr-downloads) — downloads
- [`buffr-permissions`](../../crates/buffr-permissions) — site permissions
- [`buffr-zoom`](../../crates/buffr-zoom) — per-domain zoom

## License

MIT. See [LICENSE](../../LICENSE).
