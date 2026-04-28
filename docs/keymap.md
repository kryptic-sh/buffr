# buffr default keymap (page mode)

Reference for the default page-mode bindings shipped by
`buffr_modal::Keymap::default_bindings`. All entries assume the leader key is
`\` (vim default); the leader is configurable per-profile via
`Keymap::set_leader`.

> **Defaults mirror Vieb** (stock `app/renderer/input.js`). Intentional
> divergences are flagged inline with **[buffr]**.

The engine speaks vim-flavoured chord notation. `<C-...>` = Ctrl, `<S-...>` =
Shift, `<M-...>` / `<A-...>` = Alt, `<D-...>` = Super (Cmd on macOS), `<leader>`
= configured leader char.

## Modes

| Mode      | Trigger           | Notes                                                  |
| --------- | ----------------- | ------------------------------------------------------ |
| `Normal`  | initial / `<Esc>` | Default; bindings below.                               |
| `Visual`  | (Phase 3)         | Selection-bearing motions. `<Esc>` returns to Normal.  |
| `Command` | `:` or `e`        | Command line / omnibar focused. `<Esc>` returns.       |
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

| Keys         | Action               | Notes |
| ------------ | -------------------- | ----- |
| `j`          | `ScrollDown(1)`      |       |
| `k`          | `ScrollUp(1)`        |       |
| `h`          | `ScrollLeft(1)`      |       |
| `l`          | `ScrollRight(1)`     |       |
| `<Down>`     | `ScrollDown(1)`      |       |
| `<Up>`       | `ScrollUp(1)`        |       |
| `<Left>`     | `ScrollLeft(1)`      |       |
| `<Right>`    | `ScrollRight(1)`     |       |
| `<C-e>`      | `ScrollDown(1)`      |       |
| `<C-y>`      | `ScrollUp(1)`        |       |
| `<C-d>`      | `ScrollHalfPageDown` |       |
| `<C-u>`      | `ScrollHalfPageUp`   |       |
| `<C-f>`      | `ScrollFullPageDown` |       |
| `<C-b>`      | `ScrollFullPageUp`   |       |
| `<PageDown>` | `ScrollFullPageDown` |       |
| `<PageUp>`   | `ScrollFullPageUp`   |       |
| `gg`         | `ScrollTop`          |       |
| `G`          | `ScrollBottom`       |       |
| `<Home>`     | `ScrollTop`          |       |
| `<End>`      | `ScrollBottom`       |       |

### Tabs

| Keys     | Action         | Notes                                           |
| -------- | -------------- | ----------------------------------------------- |
| `H`      | `TabPrev`      | **[buffr]** Vieb uses `H` for history-back.     |
| `L`      | `TabNext`      | **[buffr]** Vieb uses `L` for history-forward.  |
| `gt`     | `TabNext`      |                                                 |
| `gT`     | `TabPrev`      |                                                 |
| `t`      | `TabNew`       |                                                 |
| `d`      | `TabClose`     |                                                 |
| `<C-w>c` | `TabClose`     |                                                 |
| `<C-w>n` | `DuplicateTab` |                                                 |
| `<C-w>p` | `PinTab`       | **[buffr]** Vieb uses `<C-w>p` for prev-buffer. |

`TabClose` (and `:q`) close the active tab. The application only exits when the
last tab is gone. `DuplicateTab` clones the active tab's URL into a fresh tab;
`PinTab` toggles the pinned bit (sort hint only — pin does not prevent close).
See [`multi-tab.md`](./multi-tab.md).

### History

| Keys    | Action           | Notes                                       |
| ------- | ---------------- | ------------------------------------------- |
| `J`     | `HistoryBack`    | **[buffr]** Vieb uses `J` for next-tab.     |
| `K`     | `HistoryForward` | **[buffr]** Vieb uses `K` for previous-tab. |
| `<C-o>` | `HistoryBack`    |                                             |
| `<C-i>` | `HistoryForward` |                                             |

### Reload / stop

| Keys    | Action        | Notes                                                                |
| ------- | ------------- | -------------------------------------------------------------------- |
| `r`     | `Reload`      |                                                                      |
| `R`     | `ReloadHard`  |                                                                      |
| `<C-r>` | `ReloadHard`  |                                                                      |
| `<Esc>` | `StopLoading` |                                                                      |
| `<C-c>` | `StopLoading` | **[buffr]** Vieb uses `<C-c>` for copyText; buffr keeps StopLoading. |

### Omnibar / command line

| Keys    | Action            | Notes                                        |
| ------- | ----------------- | -------------------------------------------- |
| `e`     | `OpenOmnibar`     |                                              |
| `<C-l>` | `OpenOmnibar`     |                                              |
| `o`     | `OpenOmnibar`     | **[buffr]** kept as alias (no Vieb default). |
| `:`     | `OpenCommandLine` |                                              |
| `;`     | `OpenCommandLine` | **[buffr]** alias; Vieb uses `;` for hints.  |

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

| Keys    | Action      | Notes                                                                                    |
| ------- | ----------- | ---------------------------------------------------------------------------------------- |
| `+`     | `ZoomIn`    |                                                                                          |
| `-`     | `ZoomOut`   |                                                                                          |
| `_`     | `ZoomOut`   |                                                                                          |
| `=`     | `ZoomReset` | **[buffr]** Vieb maps `=` to zoomIn; buffr keeps `=` as ZoomReset (more useful default). |
| `<C-0>` | `ZoomReset` |                                                                                          |

### DevTools

| Keys      | Action         |
| --------- | -------------- |
| `<F12>`   | `OpenDevTools` |
| `<C-S-i>` | `OpenDevTools` |

## Mode transitions

The engine reads the resolved [`PageAction`] and auto-transitions:

- `OpenOmnibar`, `OpenCommandLine` → `Command`
- `EnterHintMode`, `EnterHintModeBackground` → `Hint`
- `EnterEditMode` → `Edit` (trie bypassed; `feed_edit_mode_key` takes over)
- `EnterMode(m)` → `m`

`<Esc>` is bound in Normal to `StopLoading` and in Visual / Command / Hint to
`EnterMode(Normal)` so every mode has a guaranteed escape hatch.

## In-overlay shortcuts (command line / omnibar)

When `:` opens the command line or `e`/`o` opens the omnibar, all keystrokes
route to the input bar instead of the page-mode trie. The bindings below mirror
readline / vim's command-line conventions.

| Keys                 | Action                                                   |
| -------------------- | -------------------------------------------------------- |
| `<Esc>` / `<C-c>`    | Cancel — close overlay, return to Normal mode.           |
| `<CR>`               | Confirm — dispatch the command or navigate to the URL.   |
| `<Tab>` / `<Down>`   | Move suggestion selection one row down (cycles to last). |
| `<S-Tab>` / `<Up>`   | Move suggestion selection one row up (clears at top).    |
| `<Left>` / `<Right>` | Move cursor through the buffer.                          |
| `<BS>`               | Delete the codepoint before the cursor.                  |
| `<C-u>`              | Clear the entire buffer.                                 |
| `<C-w>`              | Delete the word before the cursor.                       |

## In-prompt shortcuts (permissions)

When a page asks for a permission (camera, microphone, geolocation,
notifications, clipboard, MIDI sysex, …) buffr surfaces a prompt strip and
routes keystrokes to it until the request is resolved. The page content does not
see these keys.

| Keys      | Action                                          |
| --------- | ----------------------------------------------- |
| `a` / `y` | Allow once (no row written).                    |
| `A` / `Y` | Allow + remember for this origin.               |
| `d` / `n` | Deny once (no row written).                     |
| `D` / `N` | Deny + remember for this origin.                |
| `s`       | Synonym for `D` — deny + remember.              |
| `<Esc>`   | Defer — `Dismiss` / `cancel()`, no persistence. |

If multiple requests pile up they queue; the statusline shows `(N more pending)`
on the prompt strip. After resolving one the next prompt appears on the
following frame.

See
[`crates/buffr-permissions/README.md`](../crates/buffr-permissions/README.md)
for the decision-precedence rules.

## Vieb chords intentionally NOT mapped

The following Vieb normal-mode actions have no buffr `PageAction` equivalent and
are skipped until those features land:

| Vieb chord(s)           | Vieb action              | Reason not mapped                                                    |
| ----------------------- | ------------------------ | -------------------------------------------------------------------- |
| `p` / `P`               | openFromClipboard        | No `OpenFromClipboard` action                                        |
| `v`                     | startVisualSelect        | Visual mode not yet wired                                            |
| `<C-v>`                 | toVisualMode             | Visual mode not yet wired                                            |
| `<C-p>`                 | previousTab (pointer)    | Pointer mode not implemented                                         |
| `<C-n>`                 | nextTab (pointer)        | Pointer mode not implemented                                         |
| `m` / `M`               | setMark / restoreMark    | Marks not implemented                                                |
| `<C-s>`                 | downloadLink             | No `DownloadLink` action                                             |
| `<C-f>` (pointer)       | scrollPageDown (pointer) | Pointer mode not implemented                                         |
| `s` / `S`               | toSearchMode (special)   | Covered by `/` / `?`                                                 |
| `<C-a>` / `<C-x>`       | incrementUrl / decrement | No URL increment action                                              |
| `<kPlus>` / `<kMinus>`  | zoomIn / zoomOut         | `kPlus`/`kMinus` not a named key in buffr parser; covered by `+`/`-` |
| `<C-t>`                 | openNewTab               | Covered by `t`                                                       |
| `u`                     | reopenTab                | No `ReopenTab` action                                                |
| `<C-Tab>` / `<C-S-Tab>` | nextTab / prevTab        | Covered by `J`/`K`                                                   |

## Customising

Bindings come from a static table in `crates/buffr-modal/src/keymap.rs`. User
overrides go in `~/.config/buffr/config.toml` under `[keymap.<mode>]` — see
[`config.md`](./config.md) for the full schema and action notation. The watcher
reloads the keymap on file changes (250ms debounced).
