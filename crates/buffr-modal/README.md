# buffr-modal

Vim-style modal keybinding engine for buffr.

[![CI](https://github.com/kryptic-sh/buffr/actions/workflows/ci.yml/badge.svg)](https://github.com/kryptic-sh/buffr/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](../../LICENSE)
[![Website](https://img.shields.io/badge/website-buffr.kryptic.sh-7ee787)](https://buffr.kryptic.sh)

Two-layer modal model for a browser:

- **Page mode** (`PageMode`) — Normal / Insert / Visual / Command / Hint /
  Pending. Scroll, tab switch, omnibar, hint mode, command line. Driven by a
  prefix-trie keymap.
- **Insert mode** — typing in `<textarea>` / `contenteditable` / form fields.
  Delegates to `hjkl_engine::Editor` against a mirrored `hjkl_buffer::Buffer`
  synced to the DOM via CEF console-IPC.

## Status

`0.0.1` — `Engine` + `Keymap` trie stable; full default binding table with
ambiguity timeout; `EditSession` wiring `hjkl-engine` 0.1.0 for insert-mode text
editing; winit `KeyEvent` adapter behind optional `winit` feature.

## Features

| Feature | Default | Notes                                                            |
| ------- | ------- | ---------------------------------------------------------------- |
| `winit` | off     | Enables `key_event_to_chord` / `key_event_to_chord_with_repeat`. |

## Usage

```toml
# Cargo.toml
buffr-modal = { path = "crates/buffr-modal" }

# With winit adapter:
buffr-modal = { path = "crates/buffr-modal", features = ["winit"] }
```

```rust,no_run
// pseudo-code — see apps/buffr/src/main.rs for full integration

use buffr_modal::{Engine, Step, Keymap, PageMode};

let keymap = Keymap::default_bindings(' ');  // space as leader
let mut engine = Engine::new(keymap);

// Feed a chord sequence from your event loop.
// Engine returns Step::Action(action) when a binding fires.
// let step = engine.feed(chord);
```

## Public API

| Type / function                    | Purpose                                                              |
| ---------------------------------- | -------------------------------------------------------------------- |
| `Engine`                           | Page-mode FSM. Owns a `Keymap` and current `PageMode`.               |
| `Step`                             | Outcome of feeding one `KeyChord`: `Action`, `Consumed`, `Pass`.     |
| `EditModeStep`                     | Outcome of an edit-mode keystroke: `Action`, `Consumed`, `Nop`.      |
| `PageMode`                         | FSM state: `Normal / Visual / Command / Hint / Pending / Insert`.    |
| `Mode`                             | Coarse statusline label (same variants minus `Pending`).             |
| `PageAction`                       | Browser command emitted by the engine (scroll, tab, omnibar, …).     |
| `Keymap`                           | Prefix-trie per-mode binding store.                                  |
| `Keymap::default_bindings(leader)` | Build the full default keymap for the given leader char.             |
| `Keymap::bind(mode, keys, action)` | Add / overwrite one binding.                                         |
| `Keymap::entries(mode)`            | Flatten to `(chord_sequence, action)` pairs (new-tab page listing).  |
| `EditSession`                      | Wraps `hjkl_engine::Editor` for insert-mode DOM text editing.        |
| `BuffrHost`                        | `hjkl_engine::Host` adapter that maps engine callbacks to buffr IPC. |
| `KeyChord` / `Modifiers` / `Key`   | Parsed key representation.                                           |
| `parse_keys(str)`                  | Parse a vim-notation key sequence (`"<C-w>j"`, `"gg"`, …).           |
| `key_event_to_chord` (winit)       | Translate `winit::KeyEvent` → `KeyChord`.                            |

## Default Normal-mode bindings (excerpt)

| Key(s)                | Action                          |
| --------------------- | ------------------------------- |
| `j` / `k` / `h` / `l` | Scroll down / up / left / right |
| `<C-d>` / `<C-u>`     | Half-page down / up             |
| `gg` / `G`            | Scroll to top / bottom          |
| `H` / `L`             | Prev / next tab                 |
| `gt` / `gT`           | Next / prev tab                 |
| `d` / `<C-w>`         | Close tab                       |
| `o` / `O`             | Open tab right / left           |
| `u`                   | Reopen closed tab               |
| `J` / `K`             | History back / forward          |
| `r` / `R`             | Reload / hard-reload            |
| `e` / `<C-l>`         | Open omnibar                    |
| `:` / `;`             | Command line                    |
| `f` / `F`             | Hint mode (foreground / bg tab) |
| `/` / `?`             | Find forward / backward         |
| `n` / `N`             | Find next / prev                |
| `y`                   | Yank page URL                   |
| `+` / `-` / `0`       | Zoom in / out / reset           |
| `i` / `gi`            | Focus first input (Insert mode) |
| `<Esc>`               | Exit Insert mode                |

Full table: see [`src/keymap.rs`](src/keymap.rs) `DEFAULT_BINDINGS` constant or
run `buffr --audit-keymap`.

## License

MIT. See [LICENSE](../../LICENSE).
