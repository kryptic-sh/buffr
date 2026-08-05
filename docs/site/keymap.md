# buffr default keymap (page mode)

Reference for the default page-mode bindings shipped by
`buffr_modal::Keymap::default_bindings`.

**Leader key:** the default is a single **space** (`general.leader = " "` in
`Config`), so the one `<leader>` binding below (`<leader>p` → `PinTab`) is typed
as `<Space>p` out of the box. Set `[general] leader = "\\"` for the vim
convention; `build_keymap` feeds that character to `Keymap::default_bindings`,
so every `<leader>` chord follows the config. (`buffr --audit-keymap` prints the
raw table strings, so a leader chord shows as the literal token `<leader>p`
rather than the key you actually press — `Keymap::audit_default_bindings`
ignores the leader it is handed. Cosmetic only.)

> **Defaults mirror Vieb** (stock `app/renderer/input.js`). Intentional
> divergences are flagged inline with **[buffr]**.

The engine speaks vim-flavoured chord notation. `<C-...>` = Ctrl, `<S-...>` =
Shift, `<M-...>` / `<A-...>` = Alt, `<D-...>` = Super (Cmd on macOS), `<leader>`
= configured leader char.

## Modes

| Mode      | Trigger           | Notes                                                   |
| --------- | ----------------- | ------------------------------------------------------- |
| `Normal`  | initial / `<Esc>` | Default; bindings below.                                |
| `Visual`  | left-drag ≥ 4 px  | Text selection in the page. `y` yanks, `<Esc>` cancels. |
| `Command` | `:` or `e`        | Command line / omnibar focused. `<Esc>` returns.        |
| `Hint`    | `f` / `F`         | DOM hint overlay active. `<Esc>` returns.               |
| `Pending` | (transient)       | Multi-key prefix in flight. Not user-bindable.          |
| `Insert`  | text-field focus  | Forwarded to `Engine::feed_edit_mode_key`.              |

## Count prefix

- Leading digits accumulate: `5j` scrolls down 5 lines, `12G` jumps to line 12
  (when implemented). `0` alone is bindable (vim convention: column 0); digits
  1-9 always start a count.

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

| Keys        | Action                      | Notes                                                                         |
| ----------- | --------------------------- | ----------------------------------------------------------------------------- |
| `H`         | `TabPrev`                   | **[buffr]** Vieb uses `H` for history-back.                                   |
| `L`         | `TabNext`                   | **[buffr]** Vieb uses `L` for history-forward.                                |
| `gt`        | `TabNext`                   |                                                                               |
| `gT`        | `TabPrev`                   |                                                                               |
| `o`         | `TabNewRight`               | **[buffr]** Open tab to the right of active; omnibar opens so you type a URL. |
| `O`         | `TabNewLeft`                | **[buffr]** Open tab to the left of active; omnibar opens so you type a URL.  |
| `<C-t>`     | `TabNewRight`               | Conventional-browser alternate for `o`.                                       |
| `d`         | `TabClose`                  |                                                                               |
| `<C-w>`     | `TabClose`                  | Deliberately a **leaf** — there are no `<C-w>X` prefix chords.                |
| `u`         | `ReopenClosedTab`           | Stack-based: repeated `u` undoes successive closes.                           |
| `<C-S-t>`   | `ReopenClosedTab`           | Conventional-browser alternate for `u`.                                       |
| `<leader>p` | `PinTab`                    | Default leader is space, i.e. `<Space>p`.                                     |
| `p`         | `PasteUrl { after: true }`  | Open the clipboard URL in a tab to the right. Non-URL clipboard = no-op.      |
| `P`         | `PasteUrl { after: false }` | Same, to the left.                                                            |
| `<C-S-h>`   | `MoveTabLeft`               | Shuffle the active tab one slot left.                                         |
| `<C-S-l>`   | `MoveTabRight`              | Shuffle the active tab one slot right.                                        |

`TabClose` (and `:q`) close the active tab. The application only exits when the
last tab is gone. `PinTab` toggles the pinned bit (pinned tabs sort to the front
— pin does not prevent close). There is **no** `PageAction` for duplicating a
tab — the capability exists only as the "Duplicate Tab" entry in the tab-strip
right-click menu (`ContextMenuItem::TabDuplicate`). See
[`multi-tab.md`](./multi-tab.md) and [`context-menu.md`](./context-menu.md).

### History

| Keys    | Action           | Notes                                       |
| ------- | ---------------- | ------------------------------------------- |
| `J`     | `HistoryBack`    | **[buffr]** Vieb uses `J` for next-tab.     |
| `K`     | `HistoryForward` | **[buffr]** Vieb uses `K` for previous-tab. |
| `<C-o>` | `HistoryBack`    |                                             |
| `<C-i>` | `HistoryForward` |                                             |

### Reload / stop

| Keys    | Action       | Notes |
| ------- | ------------ | ----- |
| `r`     | `Reload`     |       |
| `R`     | `ReloadHard` |       |
| `<C-r>` | `ReloadHard` |       |

> **Note:** `<Esc>` is not bound to `StopLoading` in Normal mode; it is
> `ExitInsertMode` — it blurs the focused DOM element and resets the engine to
> Normal unconditionally.
>
> **`<C-c>` is `StopLoading`** (a buffr extension); `y` is `YankUrl`.

### Omnibar / command line

| Keys    | Action            | Notes                                       |
| ------- | ----------------- | ------------------------------------------- |
| `e`     | `OpenOmnibar`     |                                             |
| `<C-l>` | `OpenOmnibar`     |                                             |
| `:`     | `OpenCommandLine` |                                             |
| `;`     | `OpenCommandLine` | **[buffr]** alias; Vieb uses `;` for hints. |

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

| Keys | Action    | Notes |
| ---- | --------- | ----- |
| `y`  | `YankUrl` |       |

### Zoom

| Keys    | Action      | Notes                                          |
| ------- | ----------- | ---------------------------------------------- |
| `+`     | `ZoomIn`    |                                                |
| `=`     | `ZoomIn`    | Matches Chromium's `Ctrl+=` alias for zoom-in. |
| `-`     | `ZoomOut`   |                                                |
| `_`     | `ZoomOut`   |                                                |
| `0`     | `ZoomReset` |                                                |
| `)`     | `ZoomReset` |                                                |
| `<C-0>` | `ZoomReset` | Vieb-style alias for the conventional chord.   |

### DevTools

| Keys      | Action         |
| --------- | -------------- |
| `<F12>`   | `OpenDevTools` |
| `<C-S-i>` | `OpenDevTools` |

### Insert mode

| Keys    | Action            | Notes                                                                                    |
| ------- | ----------------- | ---------------------------------------------------------------------------------------- |
| `i`     | `FocusFirstInput` | **[buffr]** Same as `gi` — JS focuses first form input; focusin auto-promotes to Insert. |
| `gi`    | `FocusFirstInput` | **[buffr]** Vieb's `insertAtFirstInput`. JS focuses first input; focusin auto-promotes.  |
| `<Esc>` | `ExitInsertMode`  | Blurs the active DOM element; resets edit state and engine to Normal unconditionally.    |

> `EnterInsertMode` remains in the action enum for advanced user config (e.g.
> `[keymap.normal] "<F2>" = "enter_insert_mode"`) but is unbound by default.

## Visual-mode bindings

Visual mode is entered automatically by dragging with the left mouse button in
the page area (more than a 4 px threshold); the embedded CEF view renders the
selection itself. There is no key that enters Visual mode by default.

| Keys    | Action              | Notes                                                      |
| ------- | ------------------- | ---------------------------------------------------------- |
| `y`     | `YankSelection`     | Copies the page selection via CEF's native `frame.copy()`. |
| `<C-c>` | `YankSelection`     | Same.                                                      |
| `<Esc>` | `EnterMode(Normal)` | Cancels without yanking.                                   |

## Hint- and Command-mode bindings

| Mode      | Keys    | Action              |
| --------- | ------- | ------------------- |
| `Hint`    | `<Esc>` | `EnterMode(Normal)` |
| `Command` | `<Esc>` | `EnterMode(Normal)` |

Every other keystroke in those modes is consumed by the hint filter or the input
bar — see the overlay table below.

## Mode transitions

The engine reads the resolved [`PageAction`] and auto-transitions:

- `OpenOmnibar`, `OpenCommandLine` → `Command`
- `EnterHintMode`, `EnterHintModeBackground` → `Hint`
- `EnterInsertMode` → `Insert` (trie bypassed; `feed_edit_mode_key` takes over)
- `ExitInsertMode` → `Normal` (blurs DOM active element; clears EditFocus)
- `EnterMode(m)` → `m`

`<Esc>` is bound in Normal to `ExitInsertMode` and in Visual / Command / Hint to
`EnterMode(Normal)` so every mode has a guaranteed escape hatch.

## In-overlay shortcuts (command line / omnibar)

When `:` opens the command line or `e`/`<C-l>` opens the omnibar, all keystrokes
route to the input bar instead of the page-mode trie. The bindings below mirror
readline / vim's command-line conventions.

| Keys                 | Action                                                   |
| -------------------- | -------------------------------------------------------- |
| `<Esc>` / `<C-c>`    | Cancel — close overlay, return to Normal mode.           |
| `<CR>`               | Confirm — dispatch the command or navigate to the URL.   |
| `<Tab>` / `<Down>`   | Move suggestion selection one row down (clamps at last). |
| `<S-Tab>` / `<Up>`   | Move suggestion selection one row up (clears at top).    |
| `<Left>` / `<Right>` | Move cursor through the buffer.                          |
| `<BS>`               | Delete the codepoint before the cursor.                  |
| `<C-u>`              | Clear the entire buffer.                                 |
| `<C-w>`              | Delete the word before the cursor.                       |
| `<C-v>`              | Paste clipboard text, with CR/LF stripped.               |
| `<Space>`            | Literal space (the toolkit reports it as a named key).   |

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
[`crates/buffr-permissions/README.md`](../../crates/buffr-permissions/README.md)
for the decision-precedence rules.

## Mouse / context menu

| Gesture / input         | Action                                                                                             |
| ----------------------- | -------------------------------------------------------------------------------------------------- |
| Right-click (page area) | Open context menu. Items depend on the hit-test target (see [context-menu.md](./context-menu.md)). |
| `<Up>` / `<Down>`       | Move row selection in the open menu.                                                               |
| `<Enter>`               | Activate selected menu item.                                                                       |
| `<Esc>`                 | Dismiss menu without action.                                                                       |
| Click outside panel     | Dismiss menu without action.                                                                       |
| Any non-navigation key  | Dismiss menu and pass key to normal page-mode dispatcher.                                          |
| Left-click (tab strip)  | Switch tab and close the omnibar overlay (parity with `gt`/`gT`).                                  |
| Two-finger swipe right  | `HistoryBack` (≥ 150 px horizontal, 2× more horiz than vertical).                                  |
| Two-finger swipe left   | `HistoryForward` (same threshold).                                                                 |

## Vieb chords intentionally NOT mapped

The following Vieb normal-mode actions have no buffr `PageAction` equivalent and
are skipped until those features land:

| Vieb chord(s)           | Vieb action              | Reason not mapped                                                    |
| ----------------------- | ------------------------ | -------------------------------------------------------------------- |
| `v`                     | startVisualSelect        | Visual mode is mouse-entered; no keyboard entry chord yet            |
| `<C-v>`                 | toVisualMode             | Same                                                                 |
| `<C-p>`                 | previousTab (pointer)    | Pointer mode not implemented                                         |
| `<C-n>`                 | nextTab (pointer)        | Pointer mode not implemented                                         |
| `m` / `M`               | setMark / restoreMark    | Marks not implemented                                                |
| `<C-s>`                 | downloadLink             | No `DownloadLink` action                                             |
| `s` / `S`               | toSearchMode (special)   | Covered by `/` / `?`                                                 |
| `<C-a>` / `<C-x>`       | incrementUrl / decrement | No URL increment action                                              |
| `<kPlus>` / `<kMinus>`  | zoomIn / zoomOut         | `kPlus`/`kMinus` not a named key in buffr parser; covered by `+`/`-` |
| `<C-Tab>` / `<C-S-Tab>` | nextTab / prevTab        | Covered by `H`/`L` and `gt`/`gT`                                     |

Note that `p` / `P`, `u`, `<C-t>`, and `<C-f>` **are** bound — see the Tabs and
Scroll tables above. Only their Vieb _semantics_ differ: buffr's `p`/`P` paste a
clipboard URL into a new tab, `u` reopens the last closed tab, `<C-t>` opens a
tab to the right, and `<C-f>` is a full-page scroll (Vieb's pointer-mode variant
is what is unmapped).

## Customising

Bindings come from a static table in `crates/buffr-modal/src/keymap.rs`. User
overrides go in `~/.config/buffr/config.toml` under `[keymap.<mode>]` — see
[`config.md`](./config.md) for the full schema and action notation. The watcher
reloads the keymap on file changes (250ms debounced).
