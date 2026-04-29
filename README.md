# buffr-ui

Browser chrome for buffr — statusline, tab strip, input bar, permission and
confirm prompts.

[![CI](https://github.com/kryptic-sh/buffr/actions/workflows/ci.yml/badge.svg)](https://github.com/kryptic-sh/buffr/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](../../LICENSE)
[![Website](https://img.shields.io/badge/website-buffr.kryptic.sh-7ee787)](https://buffr.kryptic.sh)

Pure pixel-blit rendering into a `&mut [u32]` slice (BGRA, row-major). No
`winit` or `softbuffer` types in the public API — callers (`apps/buffr`) own the
surface lifecycle and pass a raw pixel buffer each frame. The chrome lives in a
strip docked to the bottom of the buffr window, below the CEF child window. GPU
upload is done by the wgpu layer in `apps/buffr/src/render.rs`.

## Status

`0.0.1` — `Statusline` (mode block, URL cell, cert pip, progress bar, zoom, find
count, hint state, update indicator, private marker, high-contrast mode);
`TabStrip` (tab labels, active/pinned indicators); `InputBar` (omnibar +
command-line input with suggestions); `PermissionsPrompt` and `ConfirmPrompt`.

## Components

| Component             | Height constant             | Purpose                                                     |
| --------------------- | --------------------------- | ----------------------------------------------------------- |
| `Statusline`          | `STATUSLINE_HEIGHT = 30`    | Mode indicator + URL + cert pip + right-hand cells.         |
| `TabStrip`            | `TAB_STRIP_HEIGHT`          | Tab labels; active and pinned markers.                      |
| `InputBar`            | `INPUT_HEIGHT`              | Omnibar / command-line input; up to `MAX_SUGGESTIONS` rows. |
| `PermissionsPrompt`   | `PERMISSIONS_PROMPT_HEIGHT` | Inline permission decision prompt.                          |
| `ConfirmPrompt`       | `CONFIRM_PROMPT_HEIGHT`     | Generic yes/no confirm prompt (e.g., close-all-tabs).       |
| `DownloadNoticeStrip` | `DOWNLOAD_NOTICE_HEIGHT`    | Ephemeral download started / finished notification strip.   |

## Usage

```toml
# Cargo.toml (workspace path dep)
buffr-ui = { path = "crates/buffr-ui" }
```

```rust,no_run
// pseudo-code — see apps/buffr/src/main.rs for the actual wiring

use buffr_ui::{Statusline, STATUSLINE_HEIGHT};

let s = Statusline {
    mode: buffr_modal::PageMode::Normal,
    url: "https://example.com".into(),
    progress: 1.0,
    ..Statusline::default()
};

// `buffer` is the full window's BGRA pixel slice (one u32 per pixel, row-major).
// `width` / `height` are the full window dimensions in pixels.
s.paint(&mut buffer, width, height);
// The statusline occupies only the bottom `STATUSLINE_HEIGHT` rows;
// the CEF child window above is untouched.
```

## Colour model

Pixels are `u32` with layout `0xFF_RR_GG_BB` (little-endian byte order
`[B, G, R, A]`), matching `wgpu::TextureFormat::Bgra8Unorm`. Alpha is always
`0xFF` (fully opaque) so GPU alpha-blending composites chrome strips over the
OSR texture correctly.

Mode-specific accent colours:

| Mode    | Accent (`0x00_RR_GG_BB`) |
| ------- | ------------------------ |
| Normal  | `4A_C9_5C` (green)       |
| Visual  | `E0_8B_2A` (amber)       |
| Command | `55_88_FF` (blue)        |
| Hint    | `C8_5A_E0` (violet)      |
| Insert  | `5A_AA_E0` (cyan)        |

High-contrast palette (`theme.high_contrast = true`): `HC_BG = 0xFF_00_00_00`
(black), `HC_FG = 0xFF_FF_FF_FF` (white), `HC_ACCENT = 0xFF_FF_FF_00` (yellow).

## Font

`buffr_ui::font` — bitmap glyph rasteriser; ASCII printable range.
`text_width(s)` returns pixel width;
`draw_text(buffer, width, height, x, y, text, colour)` blits glyphs directly. No
font file dependency; glyphs are compile-time const arrays.

## License

MIT. See [LICENSE](../../LICENSE).
