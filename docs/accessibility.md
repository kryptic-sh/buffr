# buffr — accessibility

Honest status: **web content** is accessible (CEF feature); **native chrome**
currently isn't. Keyboard-only operation is comprehensive. A high-contrast theme
is available.

## Web content (CEF renderer accessibility tree)

When `[accessibility] force_renderer_accessibility = true`, buffr's
`App::on_before_command_line_processing` injects the
`--force-renderer-accessibility` Chromium switch. This causes the renderer to
build the accessibility tree for every page; platform screen readers
(Orca/AT-SPI on Linux, VoiceOver/NSAccessibility on macOS, NVDA/JAWS via MSAA on
Windows) consume that tree the same way they would for Chromium proper.

The default is `false` because building the tree is a non-trivial per-frame cost
users without an AT don't need. Users who rely on a screen reader should enable
it on first launch.

The `cef` crate buffr pins (`148.x`, wrapping libcef `147.0.14`) does not expose
a `Settings::accessibility_state` field; the command-line switch path is the
supported wiring. (There is also a `SetAccessibilityState` method on the
per-browser host that can be flipped later, but the command-line switch covers
every renderer at process start.)

## Native chrome — keyboard-first, no AT bridge yet

The statusline, tab strip, command bar, omnibar, and permissions prompt are
rasterized on the CPU into a pixel buffer and uploaded to the GPU with `wgpu`
(`softbuffer` was replaced by the wgpu present layer). They are **not** part of
any DOM and are **not** exposed via platform accessibility APIs. (The hint
overlay is the exception: it is injected into the page DOM — see
[hint-mode.md](./hint-mode.md).) Real cross-platform native a11y bridges
(AT-SPI, NSAccessibility, MSAA) are substantial multi- platform work and are
deferred to post-1.0.

Until then, every chrome surface is reachable via the keyboard:

- `:` / `;` — command line
- `e` / `<C-l>` — omnibar
- `o` / `O` — new tab right / left (omnibar opens for the URL)
- `f` / `F` — hint mode (foreground / background)
- `gt` / `gT`, `H` / `L` — next / prev tab
- `d` or `<C-w>` — close tab; `u` or `<C-S-t>` — reopen closed tab
- `<Space>p` (`<leader>p`) — pin/unpin the active tab
- `<C-S-h>` / `<C-S-l>` — move the active tab left / right
- `J` / `K` (or `<C-o>` / `<C-i>`) — history back / forward
- `r` / `<C-r>` — reload / hard reload
- `/` / `?` / `n` / `N` — find / find-backwards / next-match / prev-match
- `y` — yank the URL (in Visual mode, the selection)
- `+` / `=` / `-` / `0` — zoom in / in / out / reset
- `<F12>` / `<C-S-i>` — devtools

Run `buffr --audit-keymap` to print the full table from any shell, or read
[keymap.md](./keymap.md). The `every_user_facing_action_has_a_default_binding`
unit test guards against drift: a new `PageAction` variant lands in
`buffr-modal` → either it gets a default binding or the test fails. (Caveat: the
test scans the static binding table rather than the built trie, so a chord bound
twice can mask a shadowed action — `StopLoading` is currently in that state, see
[keymap.md](./keymap.md).)

## High-contrast theme

`[theme] high_contrast = true` switches the chrome palette to:

| Token           | Default (accent-derived)   | High-contrast |
| --------------- | -------------------------- | ------------- |
| `accent`        | `#7aa2f7` (`theme.accent`) | `#ffff00`     |
| `bg`            | accent blended 92 % black  | `#000000`     |
| `bg_lifted`     | accent blended 85 % black  | `#101010`     |
| `fg`            | `#eeeeee`                  | `#ffffff`     |
| `fg_dim`        | `#a0a8ac`                  | `#c0c0c0`     |
| `cert_secure`   | `#66e08a`                  | `#ffffff`     |
| `cert_insecure` | `#e05a5a`                  | `#ffffff`     |
| `private`       | `#ffc8c8`                  | `#ffffff`     |
| `progress`      | `#66c2ff`                  | `#ffffff`     |
| `update`        | `#e0c85a`                  | `#ffffff`     |

The values pass WCAG AAA contrast against each other on the chrome surfaces.
They live in `Palette::high_contrast()` in `crates/buffr-ui/src/lib.rs`, and
they override every `[theme]` colour from the config.

## What's deferred (post-1.0)

- AT-SPI bridge for the chrome on Linux.
- NSAccessibility bridge on macOS.
- MSAA + UI Automation bridge on Windows.
- Larger-text option for the chrome font (`crates/buffr-ui/src/font.rs`
  rasterizes at a fixed `TARGET_PX = 15.0` with no user scale).
- Reduced-motion preference. There is no way to turn chrome animation off: the
  ASCII splash loading animation (`apps/buffr-app/src/loading_anim.rs`, a ~2 s
  cycle at 12 fps) and the page-load progress bar both animate unconditionally.

If any of these block your daily use, file an issue at
<https://github.com/kryptic-sh/buffr/issues>.
