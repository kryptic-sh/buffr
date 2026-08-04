//! ASCII-art loading animation drawn into the CEF browser region while the
//! OSR buffer is not yet usable (no paint received, or dimensions mismatch).
//!
//! Mirrors `hjkl_splash::presets::hjkl`: the static `art.txt` letterform is
//! the splash background and `PATH` traces the cursor down each letter's
//! vertical spine. The crate's `Splash` emits cells; we blit each as a single
//! glyph via `buffr_ui::font::draw_text`, mapping `Cursor`/`Trail` to `fg`
//! and `Art` to a half-bright dim so the wordmark sits behind the moving
//! highlight.
//!
//! `art.txt` is shared with the `buffr-app --help` figlet header.

use hjkl_splash::{CellKind, Layout, Splash};

const ART: &str = include_str!("art.txt");
const ROWS: u16 = 5;
const COLS: u16 = 41;

/// Cursor traces each letter's left vertical spine top→bottom, jumps to the
/// next letter, repeats. Cycle length = 25 ticks (~2 s at 12 fps).
#[rustfmt::skip]
const PATH: &[(u8, u8, char)] = &[
    // B spine
    (0, 0, '█'), (1, 0, '█'), (2, 0, '█'), (3, 0, '█'), (4, 0, '█'),
    // u left vertical
    (0, 8, '█'), (1, 8, '█'), (2, 8, '█'), (3, 8, '█'), (4, 8, '█'),
    // f1 spine
    (0, 17, '█'), (1, 17, '█'), (2, 17, '█'), (3, 17, '█'), (4, 17, '█'),
    // f2 spine
    (0, 25, '█'), (1, 25, '█'), (2, 25, '█'), (3, 25, '█'), (4, 25, '█'),
    // r spine
    (0, 33, '█'), (1, 33, '█'), (2, 33, '█'), (3, 33, '█'), (4, 33, '█'),
];

/// Half-bright `fg` for static art cells. Bit-shifts each RGB channel by 1
/// while preserving alpha.
#[inline]
fn dim(c: u32) -> u32 {
    (c & 0xff00_0000) | ((c & 0x00fe_fefe) >> 1)
}

/// Build a fresh `Splash` anchored at "now". `hjkl-splash` 0.2 owns the
/// time source — `cells()` reads the wall clock — so callers construct
/// once at App startup and pass the same `&Splash` to every paint call.
pub fn new_splash() -> Splash<'static> {
    Splash::new(ART, PATH)
}

/// Render the static splash wordmark as a row-major grid of spans,
/// where filled cells get `class="b"` and empty cells stay empty. The
/// page CSS draws filled cells as rectangles via `background: currentColor`,
/// so the wordmark renders without depending on the font's `█` glyph
/// having the same advance as a space — fixing the row drift caused by
/// monospace fonts that shape consecutive `█` differently from short
/// runs (top crossbars of `f`/`F` were visibly mis-aligned with the
/// vertical spines below).
pub fn splash_art_html() -> String {
    let mut grid = [[false; COLS as usize]; ROWS as usize];
    for (row_idx, line) in ART.lines().take(ROWS as usize).enumerate() {
        for (col_idx, ch) in line.chars().take(COLS as usize).enumerate() {
            grid[row_idx][col_idx] = ch == '█';
        }
    }
    render_grid_html(&grid)
}

/// Render the current splash frame's cursor layer as a row-major grid
/// of spans (same shape as [`splash_art_html`]). Mostly empty cells; only
/// cursor/trail positions get `class="b"`. Pushed into the page's
/// `#buffr-splash` overlay container per tick via execute_javascript.
pub fn splash_frame_html(splash: &Splash<'_>) -> String {
    let layout = Layout {
        origin_x: 0,
        origin_y: 0,
        rows: ROWS,
        cols: COLS,
    };
    let mut grid = [[false; COLS as usize]; ROWS as usize];
    for cell in splash.cells(layout) {
        let (cx, cy) = (cell.x as usize, cell.y as usize);
        if cx >= COLS as usize || cy >= ROWS as usize {
            continue;
        }
        if matches!(cell.kind, CellKind::Cursor | CellKind::Trail { .. }) {
            grid[cy][cx] = true;
        }
    }
    render_grid_html(&grid)
}

fn render_grid_html(grid: &[[bool; COLS as usize]; ROWS as usize]) -> String {
    let mut out = String::with_capacity(ROWS as usize * COLS as usize * 18);
    for row in grid {
        for &filled in row {
            if filled {
                out.push_str("<span class=\"b\"></span>");
            } else {
                out.push_str("<span></span>");
            }
        }
    }
    out
}

/// Paint the current animation frame into `buf`. The cells the splash
/// emits are derived from its internal wall clock; calling more or less
/// often does not accelerate the animation.
pub fn paint(
    buf: &mut [u32],
    buf_w: usize,
    buf_h: usize,
    rect: (u32, u32, u32, u32),
    splash: &Splash<'_>,
    fg: u32,
    bg: u32,
) {
    let (rx, ry, rw, rh) = rect;
    let (rx, ry, rw, rh) = (rx as usize, ry as usize, rw as usize, rh as usize);

    // 1. Background fill.
    let x1 = (rx + rw).min(buf_w);
    let y1 = (ry + rh).min(buf_h);
    for row in ry..y1 {
        let base = row * buf_w;
        if base + x1 > buf.len() {
            break;
        }
        buf[base + rx..base + x1].fill(bg);
    }
    if rw == 0 || rh == 0 {
        return;
    }

    // 2. Cell → pixel layout.
    let advance = buffr_ui::font::glyph_w() + 1;
    let gh = buffr_ui::font::glyph_h();
    let viewport_cols = (rw / advance) as u16;
    let viewport_rows = (rh / gh) as u16;
    if viewport_cols < COLS || viewport_rows < ROWS {
        return; // rect too small for the wordmark
    }
    let layout = Layout::centered(viewport_cols, viewport_rows, ROWS, COLS);

    // 3. Blit cells (splash drives its own clock).
    for cell in splash.cells(layout) {
        let color = match cell.kind {
            CellKind::Art => dim(fg),
            CellKind::Trail { .. } | CellKind::Cursor => fg,
        };
        let px = rx as i32 + cell.x as i32 * advance as i32;
        let py = ry as i32 + cell.y as i32 * gh as i32;
        buffr_ui::font::draw_char(buf, buf_w, buf_h, px, py, cell.ch, color);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paint_writes_into_buffer() {
        let w = 800;
        let h = 200;
        let mut buf = vec![0u32; w * h];
        let splash = Splash::fixed_tick(ART, PATH, 0);
        paint(
            &mut buf,
            w,
            h,
            (0, 0, w as u32, h as u32),
            &splash,
            0xff_ff_ff_ff,
            0,
        );
        assert!(buf.iter().any(|&p| p != 0));
    }

    #[test]
    fn paint_advances_with_tick() {
        let w = 800;
        let h = 200;
        let mut a = vec![0u32; w * h];
        let mut b = vec![0u32; w * h];
        let s0 = Splash::fixed_tick(ART, PATH, 0);
        let s1 = Splash::fixed_tick(ART, PATH, 1);
        paint(
            &mut a,
            w,
            h,
            (0, 0, w as u32, h as u32),
            &s0,
            0xff_ff_ff_ff,
            0,
        );
        paint(
            &mut b,
            w,
            h,
            (0, 0, w as u32, h as u32),
            &s1,
            0xff_ff_ff_ff,
            0,
        );
        assert_ne!(a, b);
    }

    #[test]
    fn paint_idempotent_within_same_tick() {
        // Splash::cells emits the same cells repeatedly within a tick window,
        // so calling paint() multiple times with the same fixed-tick splash
        // produces the same buffer — i.e. paint rate cannot accelerate the
        // animation. This is the core guarantee that fixes scroll-induced
        // speed-up.
        let w = 800;
        let h = 200;
        let mut a = vec![0u32; w * h];
        let mut b = vec![0u32; w * h];
        let splash = Splash::fixed_tick(ART, PATH, 7);
        paint(
            &mut a,
            w,
            h,
            (0, 0, w as u32, h as u32),
            &splash,
            0xff_ff_ff_ff,
            0,
        );
        paint(
            &mut b,
            w,
            h,
            (0, 0, w as u32, h as u32),
            &splash,
            0xff_ff_ff_ff,
            0,
        );
        assert_eq!(a, b);
    }

    #[test]
    fn splash_frame_html_emits_one_span_per_cell() {
        let html = splash_frame_html(&Splash::fixed_tick(ART, PATH, 0));
        let total_spans = html.matches("<span").count();
        assert_eq!(total_spans, ROWS as usize * COLS as usize);
    }

    #[test]
    fn splash_frame_html_emits_filled_class_for_cursor_cells() {
        let html = splash_frame_html(&Splash::fixed_tick(ART, PATH, 0));
        assert!(
            html.contains("class=\"b\""),
            "frame should mark at least one cursor cell as filled: {html:?}"
        );
    }

    #[test]
    fn splash_frame_html_changes_with_tick() {
        let a = splash_frame_html(&Splash::fixed_tick(ART, PATH, 0));
        let b = splash_frame_html(&Splash::fixed_tick(ART, PATH, 5));
        assert_ne!(a, b);
    }

    #[test]
    fn splash_art_html_includes_letterforms() {
        let html = splash_art_html();
        let total_spans = html.matches("<span").count();
        assert_eq!(total_spans, ROWS as usize * COLS as usize);
        // The static art has filled cells (the B/u/f/f/r letterforms).
        assert!(html.contains("class=\"b\""));
    }

    #[test]
    fn dim_halves_channels_preserves_alpha() {
        assert_eq!(dim(0xff_ff_ff_ff), 0xff_7f_7f_7f);
        assert_eq!(dim(0xff_00_00_00), 0xff_00_00_00);
    }
}
