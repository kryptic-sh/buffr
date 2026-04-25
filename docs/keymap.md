# buffr default keymap (page mode)

Reference for the default page-mode bindings shipped by
`buffr_modal::Keymap::default_bindings`. All entries assume the leader key is
`\` (vim default); the leader is configurable per-profile via
`Keymap::set_leader`.

The engine speaks vim-flavoured chord notation. `<C-...>` = Ctrl, `<S-...>` =
Shift, `<M-...>` / `<A-...>` = Alt, `<D-...>` = Super (Cmd on macOS), `<leader>`
= configured leader char.

## Modes

| Mode      | Trigger           | Notes                                                  |
| --------- | ----------------- | ------------------------------------------------------ |
| `Normal`  | initial / `<Esc>` | Default; bindings below.                               |
| `Visual`  | (Phase 3)         | Selection-bearing motions. `<Esc>` returns to Normal.  |
| `Command` | `:` or `o`        | Command line / omnibar focused. `<Esc>` returns.       |
| `Hint`    | `f` / `F`         | DOM hint overlay active. `<Esc>` returns.              |
| `Pending` | (transient)       | Multi-key prefix in flight. Not user-bindable.         |
| `Edit`    | text-field focus  | Forwarded to `hjkl_editor::Editor` once Phase 2 ships. |

## Count and register prefixes

- **Count** — leading digits accumulate: `5j` scrolls down 5 lines, `12G` jumps
  to line 12 (when implemented). `0` alone is bindable (vim convention: column
  0); digits 1-9 always start a count.
- **Register** — `"<char>` selects a register before a yank. Phase 2 captures
  register state on the engine but does not yet thread it through to actions.
  Yank-to-register lands with Phase 5.

## Ambiguity timeout

When a binding is a prefix of a longer one (`g` vs `gg`), the engine waits up to
`Engine::timeout()` (default 1000ms). If the user does not extend the prefix,
the shorter action fires.

## Normal-mode bindings

### Scroll

| Keys    | Action               |
| ------- | -------------------- |
| `j`     | `ScrollDown(1)`      |
| `k`     | `ScrollUp(1)`        |
| `h`     | `ScrollLeft(1)`      |
| `l`     | `ScrollRight(1)`     |
| `<C-d>` | `ScrollHalfPageDown` |
| `<C-u>` | `ScrollHalfPageUp`   |
| `<C-f>` | `ScrollFullPageDown` |
| `<C-b>` | `ScrollFullPageUp`   |
| `gg`    | `ScrollTop`          |
| `G`     | `ScrollBottom`       |

### Tabs

| Keys     | Action     |
| -------- | ---------- |
| `gt`     | `TabNext`  |
| `gT`     | `TabPrev`  |
| `<C-w>c` | `TabClose` |
| `t`      | `TabNew`   |

### History

| Keys | Action           |
| ---- | ---------------- |
| `H`  | `HistoryBack`    |
| `L`  | `HistoryForward` |

### Reload / stop

| Keys    | Action        |
| ------- | ------------- |
| `r`     | `Reload`      |
| `<C-r>` | `ReloadHard`  |
| `<C-c>` | `StopLoading` |

### Omnibar / command line

| Keys | Action            |
| ---- | ----------------- |
| `o`  | `OpenOmnibar`     |
| `:`  | `OpenCommandLine` |

### Hints

| Keys | Action                    |
| ---- | ------------------------- |
| `f`  | `EnterHintMode`           |
| `F`  | `EnterHintModeBackground` |

### Find

| Keys | Action                    |
| ---- | ------------------------- |
| `/`  | `Find { forward: true }`  |
| `?`  | `Find { forward: false }` |
| `n`  | `FindNext`                |
| `N`  | `FindPrev`                |

### Yank

| Keys | Action    |
| ---- | --------- |
| `y`  | `YankUrl` |

### Zoom

| Keys | Action      |
| ---- | ----------- |
| `+`  | `ZoomIn`    |
| `-`  | `ZoomOut`   |
| `=`  | `ZoomReset` |

### DevTools

| Keys      | Action         |
| --------- | -------------- |
| `<C-S-i>` | `OpenDevTools` |

## Mode transitions

The engine reads the resolved [`PageAction`] and auto-transitions:

- `OpenOmnibar`, `OpenCommandLine` → `Command`
- `EnterHintMode`, `EnterHintModeBackground` → `Hint`
- `EnterEditMode` → `Edit` (trie bypassed; `feed_edit_mode_key` takes over)
- `EnterMode(m)` → `m`

`<Esc>` is bound in Visual / Command / Hint to `EnterMode(Normal)` so every mode
has a guaranteed escape hatch.

## Customising

Bindings come from a static table in `crates/buffr-modal/src/keymap.rs`. User
overrides go in `~/.config/buffr/config.toml` under `[keymap.<mode>]` — see
[`config.md`](./config.md) for the full schema and action notation. The watcher
reloads the keymap on file changes (250ms debounced).
