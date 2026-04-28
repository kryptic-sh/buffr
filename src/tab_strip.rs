//! Tab strip — horizontal row of tab pills above the input bar /
//! statusline.
//!
//! Layout (top to bottom in the buffr window):
//!
//! ```text
//! +---------------------------------------------+
//! | input bar (when overlay open)               |
//! +---------------------------------------------+
//! | tab strip — TAB_STRIP_HEIGHT px             |
//! +---------------------------------------------+
//! | CEF child window (page area)                |
//! +---------------------------------------------+
//! | statusline — STATUSLINE_HEIGHT px           |
//! +---------------------------------------------+
//! ```
//!
//! Each tab is a fixed-width pill. The active tab uses the mode-accent
//! colour at full intensity; inactive tabs are dimmed. Pinned tabs
//! show a leading `*` glyph and sort first (sorting is the host's job;
//! this widget only renders what it's given).
//!
//! The widget owns no state and pulls no winit / softbuffer types into
//! its public API; like [`crate::Statusline`], embedders pass a raw
//! `&mut [u32]` slice each frame.
//!
//! Loading indicator: a 2-px progress bar at the bottom edge of each
//! tab. `progress >= 1.0` is treated as idle and the bar is hidden.

use crate::fill_rect;
use crate::font;

/// Tab strip strip height in pixels. 30 px gives a 10-px glyph row
/// with comfortable padding above + below plus a 2-px progress bar
/// reserved at the bottom. Bumping this requires the host window to
/// re-layout the CEF child rect.
pub const TAB_STRIP_HEIGHT: u32 = 30;

/// Minimum width of a single tab pill in pixels. With 8 px gutter the
/// layout falls back to overflow truncation when the strip is too
/// narrow.
pub const MIN_TAB_WIDTH: u32 = 80;

/// Maximum width of a single tab pill in pixels. Beyond this the
/// titles get long enough to be hard to glance at.
pub const MAX_TAB_WIDTH: u32 = 220;

/// Compact width for pinned tabs. Pinned tabs render as a square
/// icon-only pill — no title text, just the favicon (or a fallback
/// initial) — so the user can fit many anchors in a small strip.
pub const PINNED_TAB_WIDTH: u32 = 32;

/// Per-tab render input. `progress >= 1.0` hides the loading bar.
#[derive(Debug, Clone, PartialEq)]
pub struct TabView {
    pub title: String,
    pub progress: f32,
    pub pinned: bool,
    pub private: bool,
}

impl Default for TabView {
    fn default() -> Self {
        Self {
            title: String::new(),
            progress: 1.0,
            pinned: false,
            private: false,
        }
    }
}

/// Whole-strip render input. Re-create per frame — the widget owns no
/// state.
#[derive(Debug, Clone, Default)]
pub struct TabStrip {
    pub tabs: Vec<TabView>,
    pub active: Option<usize>,
}

impl TabStrip {
    /// Paint the tab strip into rows `[start_y, start_y + TAB_STRIP_HEIGHT)`
    /// of the window buffer. `width` and `height` are the full window's
    /// pixel dimensions; the caller passes `start_y` so the tab strip
    /// can sit anywhere vertically.
    ///
    /// The widget is a no-op when the strip would not fit (e.g. window
    /// shorter than `start_y + TAB_STRIP_HEIGHT`). When `tabs` is empty
    /// the strip is filled with `TAB_STRIP_BG` only — useful while a
    /// tab is being created on startup.
    pub fn paint(&self, buffer: &mut [u32], width: usize, height: usize, start_y: u32) {
        let strip_h = TAB_STRIP_HEIGHT as usize;
        let start_y = start_y as usize;
        if width == 0 || start_y + strip_h > height {
            return;
        }
        if buffer.len() < width * height {
            return;
        }

        // Background fill.
        fill_rect(
            buffer,
            width,
            height,
            0,
            start_y as i32,
            width,
            strip_h,
            TAB_STRIP_BG,
        );

        if self.tabs.is_empty() {
            return;
        }

        // Compute each tab's pixel rect. Pinned tabs are fixed-width
        // (PINNED_TAB_WIDTH) icon-only pills; unpinned tabs share the
        // remaining width equally, clamped to [MIN, MAX], with the
        // rightmost overflow truncating.
        let pinned_count = self.tabs.iter().filter(|t| t.pinned).count() as u32;
        let unpinned_count = self.tabs.len() as u32 - pinned_count;
        let pinned_total_w = pinned_count * PINNED_TAB_WIDTH;
        let gutter_total = ((self.tabs.len() as u32) + 1) * GUTTER;
        let avail_for_unpinned = (width as u32)
            .saturating_sub(pinned_total_w)
            .saturating_sub(gutter_total);
        let raw_w = avail_for_unpinned.checked_div(unpinned_count).unwrap_or(0);
        let tab_w = raw_w.clamp(MIN_TAB_WIDTH, MAX_TAB_WIDTH);

        let text_y = start_y as i32 + ((strip_h as i32 - font::GLYPH_H as i32) / 2);
        let progress_y = start_y as i32 + strip_h as i32 - 2;

        let mut x = GUTTER as i32;
        for (i, tab) in self.tabs.iter().enumerate() {
            // Cap so we don't draw past the right edge.
            let max_right = width as i32 - 1;
            if x >= max_right {
                break;
            }
            let target_w = if tab.pinned {
                PINNED_TAB_WIDTH as i32
            } else {
                tab_w as i32
            };
            let pill_w = target_w.min(max_right - x);
            let min_pill = if tab.pinned {
                PINNED_TAB_WIDTH as i32 / 2
            } else {
                MIN_TAB_WIDTH as i32 / 2
            };
            if pill_w < min_pill {
                break;
            }
            let is_active = self.active == Some(i);
            let bg = if is_active {
                TAB_BG_ACTIVE
            } else if tab.private {
                TAB_BG_PRIVATE
            } else {
                TAB_BG_INACTIVE
            };
            let fg = if is_active {
                TAB_FG_ACTIVE
            } else {
                TAB_FG_INACTIVE
            };

            fill_rect(
                buffer,
                width,
                height,
                x,
                start_y as i32,
                pill_w as usize,
                strip_h - 2,
                bg,
            );

            // Active accent stripe along the bottom edge — a single
            // bright row so the active tab pops even when colours are
            // close.
            if is_active {
                fill_rect(
                    buffer,
                    width,
                    height,
                    x,
                    start_y as i32 + strip_h as i32 - 4,
                    pill_w as usize,
                    2,
                    TAB_ACCENT_ACTIVE,
                );
            }

            if tab.pinned {
                // Default-favicon stand-in: a single capitalized letter
                // pulled from the title (or `*` if the title is empty)
                // centered in the pill. Real favicon image rendering
                // is a follow-up — this gives users an at-a-glance
                // anchor today without any network IO.
                let glyph: String = pinned_glyph(&tab.title);
                let glyph_px = font::text_width(&glyph) as i32;
                let glyph_x = x + (pill_w - glyph_px) / 2;
                font::draw_text(buffer, width, height, glyph_x, text_y, &glyph, fg);
            } else {
                // Title for unpinned tabs. Pre-truncate so the text
                // never bleeds onto the next pill.
                let max_text_px = (pill_w as usize).saturating_sub(12);
                let label = truncate_to_width(&tab.title, max_text_px);
                font::draw_text(buffer, width, height, x + 6, text_y, label, fg);
            }

            // Progress bar across the bottom edge of the pill — hidden
            // when idle (progress >= 1.0).
            let p = tab.progress.clamp(0.0, 1.0);
            if p > 0.0 && p < 1.0 {
                let bar_w = ((pill_w as f32) * p) as i32;
                fill_rect(
                    buffer,
                    width,
                    height,
                    x,
                    progress_y,
                    bar_w.max(1) as usize,
                    2,
                    TAB_PROGRESS,
                );
            }

            x += pill_w + GUTTER as i32;
        }
    }
}

/// First printable letter of a tab's title, uppercased — a default
/// "favicon" stand-in for pinned tabs. Falls back to `*` when the
/// title has no usable codepoint. When the title looks like a URL
/// (`scheme://…`) the scheme is skipped so the glyph reflects the
/// host, not the protocol.
fn pinned_glyph(title: &str) -> String {
    let body = title
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(title);
    for c in body.chars() {
        if c.is_alphanumeric() {
            return c.to_uppercase().to_string();
        }
    }
    "*".to_string()
}

/// Truncate `s` to fit in `max_px` pixels, appending `..` when it
/// didn't fit. Mirrors the logic in `Statusline::paint`'s URL cell.
fn truncate_to_width(s: &str, max_px: usize) -> &str {
    if font::text_width(s) <= max_px {
        return s;
    }
    if max_px < font::text_width("..") {
        return "";
    }
    let mut end = s.len();
    while end > 0 {
        if !s.is_char_boundary(end) {
            end -= 1;
            continue;
        }
        let prefix = &s[..end];
        if font::text_width(prefix) + font::text_width("..") <= max_px {
            return prefix;
        }
        end -= 1;
    }
    ""
}

const GUTTER: u32 = 4;

// vim-flavoured palette consistent with the Statusline accents.
const TAB_STRIP_BG: u32 = 0x10_18_20;
const TAB_BG_ACTIVE: u32 = 0x22_2E_22;
const TAB_BG_INACTIVE: u32 = 0x18_1E_22;
const TAB_BG_PRIVATE: u32 = 0x2A_18_2A;
const TAB_FG_ACTIVE: u32 = 0xEE_EE_EE;
const TAB_FG_INACTIVE: u32 = 0xA0_A8_AC;
const TAB_ACCENT_ACTIVE: u32 = 0x4A_C9_5C;
const TAB_PROGRESS: u32 = 0x66_C2_FF;

#[cfg(test)]
mod tests {
    use super::*;

    fn make_buf(w: usize, h: usize) -> Vec<u32> {
        vec![0u32; w * h]
    }

    #[test]
    fn paint_fills_strip_bg_when_no_tabs() {
        let w = 200;
        let h = 30;
        let mut buf = make_buf(w, h);
        let s = TabStrip::default();
        s.paint(&mut buf, w, h, 0);
        // Every pixel in the strip is the bg colour.
        for &px in &buf {
            assert_eq!(px, TAB_STRIP_BG);
        }
    }

    #[test]
    fn paint_active_tab_has_accent_stripe_pixel() {
        let w = 800;
        let h = TAB_STRIP_HEIGHT as usize;
        let mut buf = make_buf(w, h);
        let s = TabStrip {
            tabs: vec![
                TabView {
                    title: "one".into(),
                    ..Default::default()
                },
                TabView {
                    title: "two".into(),
                    ..Default::default()
                },
            ],
            active: Some(1),
        };
        s.paint(&mut buf, w, h, 0);
        // The accent stripe is 2 px tall starting at strip_h - 4.
        let stripe_y = h - 4;
        // Find at least one accent pixel on that row.
        let row = &buf[stripe_y * w..(stripe_y + 1) * w];
        assert!(
            row.contains(&TAB_ACCENT_ACTIVE),
            "no accent stripe pixel found on active tab row",
        );
    }

    #[test]
    fn paint_skips_when_strip_overflows_buffer() {
        let w = 100;
        let h = 10;
        let mut buf = make_buf(w, h);
        let s = TabStrip {
            tabs: vec![TabView::default()],
            active: Some(0),
        };
        s.paint(&mut buf, w, h, 0);
        assert!(buf.iter().all(|&p| p == 0));
    }

    #[test]
    fn paint_with_start_y_offset_only_touches_strip_rows() {
        let w = 200;
        let strip_h = TAB_STRIP_HEIGHT as usize;
        let h = strip_h + 10;
        let mut buf = make_buf(w, h);
        let s = TabStrip {
            tabs: vec![TabView::default()],
            active: Some(0),
        };
        s.paint(&mut buf, w, h, 10);
        // Rows 0..10 untouched.
        for y in 0..10 {
            for x in 0..w {
                assert_eq!(buf[y * w + x], 0, "row {y} touched");
            }
        }
    }

    #[test]
    fn pinned_tab_renders_distinctly_from_unpinned() {
        let w = 600;
        let h = TAB_STRIP_HEIGHT as usize;
        let mut buf_pin = make_buf(w, h);
        let mut buf_no_pin = make_buf(w, h);
        let pin = TabStrip {
            tabs: vec![TabView {
                title: "x".into(),
                pinned: true,
                ..Default::default()
            }],
            active: Some(0),
        };
        let no_pin = TabStrip {
            tabs: vec![TabView {
                title: "x".into(),
                pinned: false,
                ..Default::default()
            }],
            active: Some(0),
        };
        pin.paint(&mut buf_pin, w, h, 0);
        no_pin.paint(&mut buf_no_pin, w, h, 0);
        assert_ne!(buf_pin, buf_no_pin, "pin glyph not visible");
    }

    #[test]
    fn private_tab_uses_distinct_bg_when_inactive() {
        let w = 600;
        let h = TAB_STRIP_HEIGHT as usize;
        let mut buf_priv = make_buf(w, h);
        let mut buf_norm = make_buf(w, h);
        let priv_strip = TabStrip {
            tabs: vec![
                TabView {
                    title: "a".into(),
                    ..Default::default()
                },
                TabView {
                    title: "b".into(),
                    private: true,
                    ..Default::default()
                },
            ],
            active: Some(0),
        };
        let norm_strip = TabStrip {
            tabs: vec![
                TabView {
                    title: "a".into(),
                    ..Default::default()
                },
                TabView {
                    title: "b".into(),
                    private: false,
                    ..Default::default()
                },
            ],
            active: Some(0),
        };
        priv_strip.paint(&mut buf_priv, w, h, 0);
        norm_strip.paint(&mut buf_norm, w, h, 0);
        assert_ne!(buf_priv, buf_norm, "private bg should differ");
    }

    #[test]
    fn progress_bar_drawn_only_while_loading() {
        let w = 600;
        let h = TAB_STRIP_HEIGHT as usize;
        let mut buf_loading = make_buf(w, h);
        let mut buf_idle = make_buf(w, h);
        let loading = TabStrip {
            tabs: vec![TabView {
                title: "x".into(),
                progress: 0.5,
                ..Default::default()
            }],
            active: Some(0),
        };
        let idle = TabStrip {
            tabs: vec![TabView {
                title: "x".into(),
                progress: 1.0,
                ..Default::default()
            }],
            active: Some(0),
        };
        loading.paint(&mut buf_loading, w, h, 0);
        idle.paint(&mut buf_idle, w, h, 0);
        let progress_y = h - 2;
        let loading_row = &buf_loading[progress_y * w..(progress_y + 1) * w];
        let idle_row = &buf_idle[progress_y * w..(progress_y + 1) * w];
        assert!(loading_row.contains(&TAB_PROGRESS));
        assert!(!idle_row.contains(&TAB_PROGRESS));
    }

    #[test]
    fn truncate_returns_empty_when_too_narrow() {
        assert_eq!(truncate_to_width("hello world", 1), "");
    }

    #[test]
    fn truncate_returns_full_when_fits() {
        assert_eq!(truncate_to_width("hi", 1000), "hi");
    }

    #[test]
    fn many_tabs_truncate_at_strip_edge() {
        // Strip is too narrow to fit more than a few pills; the widget
        // must stop drawing rather than overflow.
        let w = 200;
        let h = TAB_STRIP_HEIGHT as usize;
        let mut buf = make_buf(w, h);
        let s = TabStrip {
            tabs: (0..10)
                .map(|i| TabView {
                    title: format!("tab {i}"),
                    ..Default::default()
                })
                .collect(),
            active: Some(0),
        };
        s.paint(&mut buf, w, h, 0);
        // No panic, no buffer overrun. The far-right column should be
        // either bg (if no pill reached it) or a pill colour, but never
        // an out-of-bounds value.
        let far_right = &buf[(h / 2) * w + (w - 1)];
        let allowed = [TAB_STRIP_BG, TAB_BG_ACTIVE, TAB_BG_INACTIVE];
        assert!(allowed.contains(far_right));
    }

    #[test]
    fn pinned_glyph_skips_scheme() {
        assert_eq!(pinned_glyph("https://example.com"), "E");
        assert_eq!(pinned_glyph("http://kryptic.sh"), "K");
        assert_eq!(pinned_glyph("buffr://new"), "N");
    }

    #[test]
    fn pinned_glyph_uses_title_when_no_scheme() {
        assert_eq!(pinned_glyph("GitHub"), "G");
        assert_eq!(pinned_glyph("  hello"), "H");
        assert_eq!(pinned_glyph(""), "*");
    }
}
