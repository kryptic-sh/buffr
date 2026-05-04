# buffr — configuration

User config is a single TOML file. Every key has a default; the loader emits an
error with a line/column span when a key is misspelt, unknown, or has the wrong
type. A copy-pasteable defaults-equivalent lives at
[`config.example.toml`](../config.example.toml) at the repo root.

## File location

| Platform | Path                                              |
| -------- | ------------------------------------------------- |
| Linux    | `$XDG_CONFIG_HOME/buffr/config.toml`              |
| macOS    | `~/Library/Application Support/buffr/config.toml` |
| Windows  | `%APPDATA%\buffr\config.toml`                     |

Path resolution goes through
`directories::ProjectDirs::from("sh", "kryptic", "buffr")`. Override per-run
with `--config <PATH>`.

## CLI flags

| Flag               | Effect                                                           |
| ------------------ | ---------------------------------------------------------------- |
| `--print-config`   | Print the resolved (defaults + user overrides) config; exit 0.   |
| `--check-config`   | Validate the config file; exit non-zero on parse / schema error. |
| `--config <PATH>`  | Override XDG-discovered config path.                             |
| `--homepage <URL>` | Override `general.homepage` for this run only.                   |

Both `--print-config` and `--check-config` short-circuit before CEF initializes,
so they're safe to run on a headless host.

## Schema

### `[general]`

| Key        | Type   | Default               | Notes                             |
| ---------- | ------ | --------------------- | --------------------------------- |
| `homepage` | string | `https://example.com` | Initial URL on first window.      |
| `leader`   | string | `\`                   | Exactly one character. Validated. |

### `[startup]`

| Key               | Type   | Default       | Notes                                   |
| ----------------- | ------ | ------------- | --------------------------------------- |
| `restore_session` | bool   | `false`       | Phase 5 work; parsed but no-op for now. |
| `new_tab_url`     | string | `about:blank` | URL for `tab_new`.                      |

### `[search]`

| Key              | Type   | Default      | Notes                                             |
| ---------------- | ------ | ------------ | ------------------------------------------------- |
| `default_engine` | string | `duckduckgo` | Must reference a `[search.engines.<name>]` block. |

`[search.engines.<name>]` blocks define each engine:

```toml
[search.engines.duckduckgo]
url = "https://duckduckgo.com/?q={query}"
```

`{query}` is replaced with the URL-encoded omnibar input.

### `[theme]`

| Key      | Type   | Default   | Notes                               |
| -------- | ------ | --------- | ----------------------------------- |
| `accent` | string | `#7aa2f7` | Hex color used for the status line. |
| `mode`   | enum   | `auto`    | `auto` \| `dark` \| `light`.        |

### `[privacy]`

| Key                | Type     | Default | Notes                                    |
| ------------------ | -------- | ------- | ---------------------------------------- |
| `enable_telemetry` | bool     | `false` | Reserved. buffr never sends telemetry.   |
| `clear_on_exit`    | string[] | `[]`    | Phase 5+; e.g. `["cookies", "history"]`. |

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

The inhibitor is acquired and released at runtime; no restart needed.

> **Note (v0.3.0):** The JS→Rust read-back path for `__buffr_video_active` is
> scaffolded but not yet wired end-to-end. The section parses and the inhibitor
> plumbing compiles on all platforms, but the inhibitor will not activate until
> the signal path lands in a future release.

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
- Every keymap binding's key sequence must parse via the engine's `parse_keys`,
  and its action notation must match the table above.
- Unknown top-level keys, unknown nested keys, and unknown enum variants all
  error out (`#[serde(deny_unknown_fields)]`).
