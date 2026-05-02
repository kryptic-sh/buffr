//! ASCII-art loading animation drawn into the CEF browser region while the
//! OSR buffer is not yet usable (no paint received, or dimensions mismatch).
//!
//! Design: a horizontal dot-cascade with the "buffr" wordmark centred below.
//! All frames have the same fixed character dimensions so centering math is
//! computed once at call time via `buffr_ui::font::{glyph_w, glyph_h}`.

/// Number of dot-cascade animation frames.
const FRAME_COUNT: usize = 8;

/// ASCII-art frames.  Each frame is a multi-line string; every frame has
/// exactly the same number of lines and the same width in characters.
///
/// Layout (3 rows):
///   row 0 – spinner dots (width = FRAME_COLS chars)
///   row 1 – blank separator
///   row 2 – "buffr" wordmark (centred inside FRAME_COLS cols by padding)
pub const FRAMES: &[&str] = &[
    "·  ·  ·  ·  ·  ·  ·\n \n      buffr      ",
    "◉  ·  ·  ·  ·  ·  ·\n \n      buffr      ",
    "·  ◉  ·  ·  ·  ·  ·\n \n      buffr      ",
    "·  ·  ◉  ·  ·  ·  ·\n \n      buffr      ",
    "·  ·  ·  ◉  ·  ·  ·\n \n      buffr      ",
    "·  ·  ·  ·  ◉  ·  ·\n \n      buffr      ",
    "·  ·  ·  ·  ·  ◉  ·\n \n      buffr      ",
    "·  ·  ·  ·  ·  ·  ◉\n \n      buffr      ",
];

/// Total number of animation frames.
#[inline]
pub fn frame_count() -> usize {
    FRAME_COUNT
}

/// Paint the animation frame at `frame_idx` into `buf`.
///
/// Steps:
/// 1. Fill `rect` (x, y, w, h) with `bg`.
/// 2. Render `FRAMES[frame_idx % frame_count()]` centred inside `rect`,
///    one line per `draw_text` call, using `fg` as the foreground colour.
///
/// The buffer uses the same row-major BGRA-u32 layout as the chrome CPU
/// buffer — `0xFF_RR_GG_BB` for opaque pixels.
pub fn paint(
    buf: &mut [u32],
    buf_w: usize,
    buf_h: usize,
    rect: (u32, u32, u32, u32),
    frame_idx: usize,
    fg: u32,
    bg: u32,
) {
    let (rx, ry, rw, rh) = rect;
    let rx = rx as usize;
    let ry = ry as usize;
    let rw = rw as usize;
    let rh = rh as usize;

    // --- 1. Background fill ---
    let x1 = (rx + rw).min(buf_w);
    let y1 = (ry + rh).min(buf_h);
    for row in ry..y1 {
        let base = row * buf_w;
        if base + x1 > buf.len() {
            break;
        }
        buf[base + rx..base + x1].fill(bg);
    }

    // --- 2. Render frame text ---
    let frame_str = FRAMES[frame_idx % frame_count()];
    let lines: Vec<&str> = frame_str.split('\n').collect();

    let gw = buffr_ui::font::glyph_w();
    let gh = buffr_ui::font::glyph_h();
    let advance = gw + 1; // draw_text uses glyph_w + 1 spacing

    // Total text block dimensions in pixels.
    let block_h_px = lines.len() * gh;
    // Width = widest line in chars × advance − 1 trailing gap.
    let max_cols = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    let block_w_px = if max_cols > 0 {
        max_cols * advance - 1
    } else {
        0
    };

    // Clamp to rect so we don't paint outside when the region is tiny.
    if block_w_px == 0 || block_h_px == 0 || rw == 0 || rh == 0 {
        return;
    }

    // Centre the block inside the rect.
    let text_x = rx as i32 + ((rw as i32 - block_w_px as i32) / 2).max(0);
    let text_y = ry as i32 + ((rh as i32 - block_h_px as i32) / 2).max(0);

    for (i, line) in lines.iter().enumerate() {
        let line_y = text_y + (i * gh) as i32;
        buffr_ui::font::draw_text(buf, buf_w, buf_h, text_x, line_y, line, fg);
    }
}
