# buffr — configuration

User config is a single TOML file. Every key has a default; the loader emits an
error with a line/column span when a key is misspelt, unknown, or has the wrong
type. A copy-pasteable defaults-equivalent lives at
[`config.example.toml`](../config.example.toml) at the repo root.

## File location

buffr is **XDG-everywhere**: the same path on all three platforms.

| Platform | Path                                                        |
| -------- | ----------------------------------------------------------- |
| Linux    | `$XDG_CONFIG_HOME/buffr/config.toml` (`~/.config/buffr/…`)  |
| macOS    | `$XDG_CONFIG_HOME/buffr/config.toml` (`~/.config/buffr/…`)  |
| Windows  | `%XDG_CONFIG_HOME%\buffr\config.toml` (`~\.config\buffr\…`) |

Path resolution goes through `hjkl_config::config_path::<Config>()` (see
`crates/buffr-config/src/loader.rs`). `$XDG_CONFIG_HOME` is honored on every
platform; there is no `~/Library/Application Support` or `%APPDATA%` fallback,
and buffr does **not** depend on the `directories` crate. Debug builds use
`buffr-debug` instead of `buffr` as the directory name so a dev tree never
shares state with an installed release.

Everything else the browser persists lives under `XDG_DATA_HOME` —
`~/.local/share/buffr/` by default, `~/.local/share/buffr-debug/` in debug
builds. That includes the six SQLite stores (`history.sqlite`,
`bookmarks.sqlite`, `downloads.sqlite`, `zoom.sqlite`, `permissions.sqlite`,
`favicons.sqlite`), `session.json`, `update-cache.json`, `usage-counters.json`,
`crashes/`, **and CEF's own profile tree** — cookies, `Local Storage`,
IndexedDB, and the HTTP `Cache` directory all sit in there together, because
`apps/buffr-app/src/main.rs` passes the data dir as CEF's `root_cache_path`.

That is deliberate: the XDG spec says `~/.cache` contents may be deleted at any
time without warning, and losing cookies and local storage to a `tmpfiles` sweep
is not acceptable. `~/.cache/buffr/` (`XDG_CACHE_HOME`) is still created at
startup and is used to derive the single-instance profile id, but CEF does not
store anything there.

Per-engine subtrees are namespaced as `~/.local/share/buffr/engines/<id>/`. See
[`[engines]`](#engines) for what that currently does — and does not — isolate.

Override the config file per-run with `--config <PATH>`.

## Config-related CLI flags

These are the flags that touch config specifically. They are **not** the full
CLI surface — `buffr --help` lists ~30 flags (bookmark/history/download/zoom/
permission dumps, `--private`, `--audit-keymap`, update flags, and so on).

| Flag               | Effect                                                           |
| ------------------ | ---------------------------------------------------------------- |
| `--print-config`   | Print the resolved (defaults + user overrides) config; exit 0.   |
| `--check-config`   | Validate the config file; exit non-zero on parse / schema error. |
| `--config <PATH>`  | Override the XDG-discovered config path.                         |
| `--homepage <URL>` | Override `general.homepage` for this run only.                   |
| `--engine <NAME>`  | Ignore `[engines]` and route every tab through `<NAME>` (`cef`). |
| `--private`        | In-memory stores + throwaway CEF cache; forces telemetry off.    |
| `--audit-keymap`   | Print every default-bound `PageAction` and its keys; exit 0.     |

Both `--print-config` and `--check-config` short-circuit before CEF initializes,
so they're safe to run on a headless host.

> The flags above are parsed by the **browser** binary (`buffr-app`,
> `apps/buffr-app/src/main.rs`). The `buffr` supervisor takes only
> `--heartbeat-timeout`, `--heartbeat-disable`, and `--help`/`--version`;
> everything else it forwards verbatim to the child, so `buffr --check-config`
> works from the user's point of view.

## Schema

The 13 sections below are the complete `Config` surface
(`crates/buffr-config/src/lib.rs`).

### `[general]`

| Key             | Type   | Default       | Notes                                                    |
| --------------- | ------ | ------------- | -------------------------------------------------------- |
| `homepage`      | string | `buffr://new` | Initial URL on first window.                             |
| `leader`        | string | `" "` (space) | Exactly one character. Validated.                        |
| `show_favicons` | bool   | `true`        | `false` skips favicon render **and** the CEF icon fetch. |

### `[startup]`

`restore_session = true` reopens the previous session's tabs on launch (opt-in —
default `false`). A fresh tab (`o`/`O`/`:tabnew`) opens `new_tab_url` (default
`about:blank`); the cold-start tab 0 still opens `general.homepage`.

| Key               | Type   | Default       | Notes                                                           |
| ----------------- | ------ | ------------- | --------------------------------------------------------------- |
| `restore_session` | bool   | `false`       | `true` restores the previous session's tabs on launch (opt-in). |
| `new_tab_url`     | string | `about:blank` | URL fresh tabs (`o`/`O`/`:tabnew`) open.                        |

### `[search]`

| Key              | Type   | Default      | Notes                                             |
| ---------------- | ------ | ------------ | ------------------------------------------------- |
| `default_engine` | string | `duckduckgo` | Must reference a `[search.engines.<name>]` block. |

`[search.engines.<name>]` blocks define each engine:

```toml
[search.engines.duckduckgo]
url = "https://duckduckgo.com/?q={query}"
prefix = "ddg"  # optional

[search.engines.github]
url = "https://github.com/search?q={query}"
prefix = "gh"
```

`{query}` is replaced with the URL-encoded omnibar input.

`prefix` is an optional shortcut keyword. When set, an omnibar input of
`<prefix> <query>` routes to that engine instead of `default_engine` — e.g.
`gh tokio` searches GitHub, `g rust closures` searches Google, plain `cats`
falls through to the default. Bare prefix words with no query (e.g. `g`) fall
through to the default so they still produce a useful result. Prefix collisions
across engines are rejected at config validation time.

### `[theme]`

Every colour is a 7-character `#RRGGBB` string. An unparseable value is a
**hard** config error (`--check-config` exits non-zero) — it is no longer
silently replaced by the built-in default.

| Key             | Type   | Default   | Notes                                                                     |
| --------------- | ------ | --------- | ------------------------------------------------------------------------- |
| `accent`        | string | `#7aa2f7` | Statusline mode block, omnibar caret, hint labels, active tab.            |
| `cert_secure`   | string | `#66e08a` | Secure cert indicator (lock dot, find counts).                            |
| `cert_insecure` | string | `#e05a5a` | Insecure cert indicator.                                                  |
| `private`       | string | `#ffc8c8` | `PRIVATE` marker on the statusline.                                       |
| `progress`      | string | `#66c2ff` | Page-load progress bar.                                                   |
| `update`        | string | `#e0c85a` | Update-available indicator (`* upd`).                                     |
| `high_contrast` | bool   | `false`   | Overrides every colour above; see [accessibility.md](./accessibility.md). |

### `[privacy]`

| Key                | Type     | Default                                      | Notes                                                                            |
| ------------------ | -------- | -------------------------------------------- | -------------------------------------------------------------------------------- |
| `enable_telemetry` | bool     | `false`                                      | Opt-in **local-only** counters. No network endpoint exists.                      |
| `clear_on_exit`    | string[] | `[]`                                         | Any of `cookies`, `cache`, `history`, `bookmarks`, `downloads`, `local_storage`. |
| `skip_schemes`     | string[] | `["about", "cef", "chrome", "data", "file"]` | URL schemes never recorded in history (case-insensitive).                        |

Telemetry is opt-in, local-only, and has **no network endpoint** — there is no
collector to send counters to. The only network request buffr makes by default
is the update check; see [privacy.md](./privacy.md) and
[updates.md](./updates.md).

> **Known bug in `clear_on_exit`.** `cookies`, `history`, `bookmarks`, and
> `downloads` work. `cache` and `local_storage` do not: `run_clear_on_exit`
> (`apps/buffr-app/src/main.rs`) deletes `<XDG_CACHE_HOME>/buffr/Cache` and
> `<XDG_CACHE_HOME>/buffr/Local Storage`, but CEF writes both under its
> `root_cache_path`, which is the **data** dir. The deletes therefore hit a
> directory CEF never populated and log `dir absent — skipping`.

### `[downloads]`

| Key                  | Type  | Default | Notes                                                                                    |
| -------------------- | ----- | ------- | ---------------------------------------------------------------------------------------- |
| `default_dir`        | path? | _unset_ | Unset resolves at runtime: `dirs::download_dir()`, then `$HOME/Downloads`, then the cwd. |
| `open_on_finish`     | bool  | `false` | Launch the file via `xdg-open` / `open` / `start` on completion.                         |
| `ask_each_time`      | bool  | `false` | `true` shows the OS Save-As dialog and suppresses the notification strip.                |
| `show_notifications` | bool  | `true`  | Chrome strip on download start/finish (2 s started, 4 s finished).                       |

### `[hint]`

| Key        | Type   | Default            | Notes                                                            |
| ---------- | ------ | ------------------ | ---------------------------------------------------------------- |
| `alphabet` | string | `asdfghjkl;weruio` | Label alphabet. Validated: non-empty, ASCII-only, no duplicates. |

See [hint-mode.md](./hint-mode.md).

### `[crash_reporter]`

| Key                | Type | Default | Notes                                                                 |
| ------------------ | ---- | ------- | --------------------------------------------------------------------- |
| `enabled`          | bool | `false` | Opt-in Rust panic hook writing JSON under `<data>/crashes/`.          |
| `purge_after_days` | u32  | `30`    | Cutoff for `--purge-crashes`. Must be `> 0`; `0` is rejected at load. |

### `[updates]`

| Key                    | Type   | Default            | Notes                                            |
| ---------------------- | ------ | ------------------ | ------------------------------------------------ |
| `enabled`              | bool   | `true`             | The only network request buffr makes by default. |
| `check_interval_hours` | u32    | `24`               | Must be `> 0`.                                   |
| `github_repo`          | string | `kryptic-sh/buffr` | `owner/repo` slug; shape-validated.              |

See [updates.md](./updates.md).

### `[accessibility]`

| Key                            | Type | Default | Notes                                                         |
| ------------------------------ | ---- | ------- | ------------------------------------------------------------- |
| `force_renderer_accessibility` | bool | `false` | Passes `--force-renderer-accessibility` to the CEF renderers. |

### `[engines]`

| Key         | Type    | Default | Notes                                                           |
| ----------- | ------- | ------- | --------------------------------------------------------------- |
| `default`   | string  | `cef`   | Engine id used when no rule matches. Must name an instance.     |
| `instances` | table[] | `[]`    | `[[engines.instances]]` — empty synthesises one `cef` instance. |
| `rules`     | table[] | `[]`    | `[[engines.rules]]` — ordered; first host-glob match wins.      |

```toml
[engines]
default = "cef"

[[engines.instances]]
id      = "cef"
backend = "cef"
# data_dir = "/tmp/cef-b-cache"   # accepted, but has no effect today

[[engines.rules]]
match  = "*.figma.com"
engine = "cef"
```

`backend` accepts only `"cef"`. It is the sole backend the browser can
construct: the WPE WebKit backend (`crates/buffr-webkit`) is excluded from the
workspace, Linux-only, and not built by CI. `--engine <NAME>` overrides the
whole section for one run and likewise only accepts `cef`.

#### `data_dir` and per-engine isolation

`data_dir` on an instance is parsed and plumbed all the way through —
`buffr-app` resolves it (explicit value, else `<data>/engines/<id>/`) and hands
it to the backend as `BackendOpenOptions::data_dir`. The CEF backend then
**discards it**: `BrowserHost::new_with_options`
(`crates/buffr-cef/src/host.rs`) does `let _ = data_dir;` and creates no
per-engine `RequestContext`, because CEF's Alloy runtime collapses a child
context's `cache_path` back onto the global `Default/` profile anyway
(kryptic-sh/buffr#158).

Net effect today: **every engine instance shares one on-disk profile** — the
`root_cache_path`, which is the data dir described under
[File location](#file-location). Setting `data_dir` changes nothing you can
observe. The key is kept so configs do not break when per-engine isolation lands
(it needs the Chrome runtime, not Alloy).

### `[keymap.<mode>]`

Mode is one of `normal`, `visual`, `command`, `hint`. Each entry maps a
vim-notation key sequence to a `PageAction`:

```toml
[keymap.normal]
"j" = "scroll_down"
"5j" = "scroll_down(5)"
"/" = "find(forward = true)"
"<Esc>" = "enter_mode(\"normal\")"
```

The full default keymap lives in [`keymap.md`](./keymap.md).

There is deliberately no `[keymap.insert]`. Insert mode forwards every key
straight to the page so the focused field handles typing natively — a binding
there would shadow whatever the user is typing, and the engine never consults
the keymap while in Insert anyway. The section is a hard validation error rather
than a silent no-op. Press `<Esc>` to leave Insert mode, then use a
`[keymap.normal]` binding.

#### Action notation

- **Unit variants** — bare snake_case name. `"scroll_down"`, `"reload"`,
  `"tab_close"`, etc.
- **Count-bearing scrolls** — `name(N)` where `N >= 0`. Applies to `scroll_up`,
  `scroll_down`, `scroll_left`, `scroll_right`.
- **Find** — `find(forward = true)` or `find(forward = false)`.
- **Mode transition** — `enter_mode("<mode>")` with a quoted mode name.

Anything else surfaces a validation error pointing at the offending key.

### `[idle_inhibit]`

Keeps the screen awake while video (or optionally audio) is playing in the
focused window. Backed by four platform implementations:

- **Linux Wayland** — `zwp_idle_inhibit_manager_v1` protocol.
- **Linux X11** — `org.freedesktop.ScreenSaver.Inhibit` over D-Bus.
- **macOS** — `IOPMAssertionCreateWithName(NoDisplaySleepAssertion)`.
- **Windows** — `SetThreadExecutionState(ES_DISPLAY_REQUIRED)` on a worker
  thread.

The inhibitor is acquired and released at runtime; no restart needed. The
video/audio signal comes from the JS media probe (`__buffr_media__` console
sentinel) plus CEF's audio callbacks, re-evaluated every frame in
`about_to_wait`:
`enabled && (video || (inhibit_audio_only && audio)) && (!require_focus || window_focused)`.

| Key                  | Type | Default | Notes                                                                                                                                                                           |
| -------------------- | ---- | ------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `enabled`            | bool | `true`  | Master switch. `false` disables the feature entirely — no inhibitor is ever acquired.                                                                                           |
| `inhibit_audio_only` | bool | `false` | When `true`, audio-only activity (no video) also triggers the inhibitor.                                                                                                        |
| `require_focus`      | bool | `true`  | When `true`, the inhibitor is held only while the buffr window has OS-level focus. Set to `false` to inhibit even when the window is in the background (useful for PiP setups). |

```toml
[idle_inhibit]
enabled = true
inhibit_audio_only = false
require_focus = true
```

## Hot reload

The watcher uses `notify` with a 250ms debounce. On a successful reload, the
**keymap only** is swapped on the running engine — homepage, theme, startup, and
search settings still require a restart for now (full hot-apply is Phase 5+
work). A failed reload (parse or validate error) is logged and the previous
config stays live.

## Validation rules

- `general.leader` must be exactly one character.
- `search.default_engine` must reference an existing `[search.engines.<name>]`
  block.
- `search.engines.<name>.prefix` (when set) must be non-empty and unique across
  all engines.
- `hint.alphabet` must be non-empty, ASCII-only, duplicate-free, and at least
  two characters.
- All six `[theme]` colours must parse as `#RRGGBB`. A typo is a hard error — it
  is no longer silently swapped for the built-in colour.
- `crash_reporter.purge_after_days` must be `> 0`.
- `updates.check_interval_hours` must be `> 0`; `updates.github_repo` must be an
  `owner/repo` slug.
- `engines.default` must be non-empty and name a declared (or synthesised)
  instance; instance ids must be unique; every rule's `engine` must resolve.
- Every keymap binding's key sequence must parse via the engine's `parse_keys`,
  and its action notation must match the table above.
- `[keymap.insert]` is rejected outright — Insert mode has no bindable keymap.
- Unknown top-level keys, unknown nested keys, and unknown enum variants all
  error out (`#[serde(deny_unknown_fields)]`).
