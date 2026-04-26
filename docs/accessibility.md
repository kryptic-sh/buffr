# buffr — accessibility

Honest status: **web content** is accessible (CEF feature); **native chrome**
currently isn't. Keyboard-only operation is comprehensive. A high-contrast
theme is available.

## Web content (CEF renderer accessibility tree)

When `[accessibility] force_renderer_accessibility = true`, buffr's
`App::on_before_command_line_processing` injects the
`--force-renderer-accessibility` Chromium switch. This causes the renderer to
build the accessibility tree for every page; platform screen readers
(Orca/AT-SPI on Linux, VoiceOver/NSAccessibility on macOS, NVDA/JAWS via MSAA
on Windows) consume that tree the same way they would for Chromium proper.

The default is `false` because building the tree is a non-trivial per-frame
cost users without an AT don't need. Users who rely on a screen reader should
enable it on first launch.

The cef-147 binding does not expose a `Settings::accessibility_state` field;
the command-line switch path is the supported wiring. (There is also a
`SetAccessibilityState` method on the per-browser host that can be flipped
later, but the command-line switch covers every renderer at process start.)

## Native chrome — keyboard-first, no AT bridge yet

The statusline, tab strip, command bar, omnibar, hint overlay, and permissions
prompt are software-rendered via `softbuffer`. They are **not** part of any
DOM and are **not** exposed via platform accessibility APIs. Real cross-platform
native a11y bridges (AT-SPI, NSAccessibility, MSAA) are substantial multi-
platform work and are deferred to post-1.0.

Until then, every chrome surface is reachable via the keyboard:

- `:` — command line
- `o` — omnibar
- `f` / `F` — hint mode
- `gt` / `gT` — next/prev tab
- `<C-w>c` / `<C-w>n` / `<C-w>p` — close / duplicate / pin
- `H` / `L` — back / forward
- `r` / `<C-r>` — reload / hard reload
- `/` / `?` / `n` / `N` — find / find-prev / next-match / prev-match
- `<C-S-i>` — devtools

Run `buffr --audit-keymap` to print the full table from any shell. The
`every_user_facing_action_has_a_default_binding` unit test guards this list
against drift: a new `PageAction` variant lands in `buffr-modal` → either it
gets a default binding or the test fails.

## High-contrast theme

`[theme] high_contrast = true` switches the chrome palette to:

| Token        | Default  | High-contrast |
| ------------ | -------- | ------------- |
| `bg`         | per-mode | `0x000000`    |
| `fg`         | `0xEEEEEE` | `0xFFFFFF`  |
| `accent`     | per-mode | `0xFFFF00`    |
| `accent_dim` | per-mode | `0xC0C0C0`    |

The values pass WCAG AAA contrast against each other on the chrome surfaces.
Colour values live in `crates/buffr-ui/src/lib.rs` as `HC_BG`, `HC_FG`,
`HC_ACCENT`, `HC_ACCENT_DIM`.

## What's deferred (post-1.0)

- AT-SPI bridge for the chrome on Linux.
- NSAccessibility bridge on macOS.
- MSAA + UI Automation bridge on Windows.
- Larger-text option for the bitmap font (the 6×10 glyphs in
  `crates/buffr-ui/src/font.rs` are fixed-size).
- Reduced-motion preference (currently no animations besides cursor blink).

If any of these block your daily use, file an issue at
<https://github.com/kryptic-sh/buffr/issues>.
