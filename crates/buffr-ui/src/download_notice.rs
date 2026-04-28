//! Download notification strip widget.
//!
//! Passive single-row toast surfaced above the tab strip when a
//! `ask_each_time = false` download starts or finishes. The widget is
//! purely render-time — queueing, expiry, and CEF callback dispatch live
//! in `apps/buffr` and `crates/buffr-core`.
//!
//! # Layout
//!
//! ```text
//! +---------------------------------------------------------------+
//! |v filename.zip -> /home/user/Downloads/filename.zip            |
//! +---------------------------------------------------------------+
//! ```
//!
//! Single text row plus a 2 px accent border at the top. Half the
//! height of the permissions prompt — this is a passive toast, not an
//! action prompt.
//!
//! # Icon prefixes (ASCII, font-safe)
//!
//! The bitmap font only covers ASCII 0x20–0x7e. Unicode arrows/checks
//! render as the hollow-box MISSING glyph, so we use ASCII fallbacks:
//!
//! - `Started`   → `v ` (down-arrow approximation)
//! - `Completed` → `OK `
//! - `Failed`    → `X `

use crate::{fill_rect, font};

/// Strip height in pixels. 2 px accent + 6 px top pad + 14 px glyph
/// row + 6 px bottom pad = 28.  Chosen as half of
/// `PERMISSIONS_PROMPT_HEIGHT` so the chrome footprint stays
/// proportional to the importance of the notice.
pub const DOWNLOAD_NOTICE_HEIGHT: u32 = 28;

/// Render input for [`DownloadNoticeStrip::paint`]. Mirrors the fields
/// of `buffr_core::DownloadNotice` but is decoupled so `buffr-ui` does
/// not depend on `buffr-core`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadNoticeStrip {
    /// What happened — drives icon prefix + accent colour.
    pub kind: DownloadNoticeKind,
    /// Suggested filename shown in the strip text.
    pub filename: String,
    /// Absolute path shown after the arrow (empty for `Failed`).
    pub path: String,
}

/// Mirrors `buffr_core::DownloadNoticeKind` — duplicated to keep
/// `buffr-ui` free of a `buffr-core` dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadNoticeKind {
    Started,
    Completed,
    Failed,
}

impl DownloadNoticeStrip {
    /// Paint the notice strip into `buffer` starting at `top_y`.
    ///
    /// `buffer` is the full window buffer (one `u32` per pixel,
    /// row-major); only the `DOWNLOAD_NOTICE_HEIGHT` rows from `top_y`
    /// are touched. `width` and `height` are the full window dimensions.
    pub fn paint(&self, buffer: &mut [u32], width: usize, height: usize, top_y: u32) {
        let strip_h = DOWNLOAD_NOTICE_HEIGHT as usize;
        if width == 0 || height == 0 || strip_h == 0 {
            return;
        }
        if buffer.len() < width * height {
            return;
        }
        let top = top_y as i32;
        if top >= height as i32 {
            return;
        }

        let (bg, accent) = palette(self.kind);

        // Background fill.
        fill_rect(buffer, width, height, 0, top, width, strip_h, bg);

        // 2 px accent border at the very top of the strip.
        fill_rect(buffer, width, height, 0, top, width, 2, accent);

        // Single text row, vertically centred.
        let text_x: i32 = 8;
        let text_y = top + (strip_h as i32 - font::glyph_h() as i32) / 2;

        let line = format_line(self.kind, &self.filename, &self.path);

        let right_pad: i32 = 8;
        let max_px = (width as i32 - text_x - right_pad).max(0) as usize;
        let line_trunc = truncate_to_width(&line, max_px);

        font::draw_text(buffer, width, height, text_x, text_y, line_trunc, COLOUR_FG);
    }
}

/// Build the single text row for the strip.
fn format_line(kind: DownloadNoticeKind, filename: &str, path: &str) -> String {
    match kind {
        DownloadNoticeKind::Started => {
            if path.is_empty() {
                format!("v {filename}")
            } else {
                format!("v {filename} -> {path}")
            }
        }
        DownloadNoticeKind::Completed => {
            if path.is_empty() {
                format!("OK {filename}")
            } else {
                format!("OK {filename} -> {path}")
            }
        }
        DownloadNoticeKind::Failed => format!("X {filename}"),
    }
}

/// Colour pair `(background, accent)` keyed by event kind.
fn palette(kind: DownloadNoticeKind) -> (u32, u32) {
    match kind {
        // Neutral blue tint — informational.
        DownloadNoticeKind::Started => (COLOUR_BG_STARTED, COLOUR_ACCENT_STARTED),
        // Green tint — success.
        DownloadNoticeKind::Completed => (COLOUR_BG_COMPLETED, COLOUR_ACCENT_COMPLETED),
        // Red tint — failure.
        DownloadNoticeKind::Failed => (COLOUR_BG_FAILED, COLOUR_ACCENT_FAILED),
    }
}

/// Truncate `s` to at most `max_px` pixels of rendered width. Mirrors
/// the helper in `lib.rs` and `permissions_prompt.rs`.
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

// Palette (opaque BGRA: 0xFF_RR_GG_BB) ---------------------------------

const COLOUR_FG: u32 = 0xFF_F0_E8_D8;

// Started: dark blue-grey background, blue accent.
const COLOUR_BG_STARTED: u32 = 0xFF_0E_12_18;
const COLOUR_ACCENT_STARTED: u32 = 0xFF_55_88_FF;

// Completed: dark green background, green accent.
const COLOUR_BG_COMPLETED: u32 = 0xFF_0A_14_0E;
const COLOUR_ACCENT_COMPLETED: u32 = 0xFF_4A_C9_5C;

// Failed: dark red background, red accent.
const COLOUR_BG_FAILED: u32 = 0xFF_16_0E_0E;
const COLOUR_ACCENT_FAILED: u32 = 0xFF_E0_5A_5A;

// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_buf(w: usize, h: usize) -> Vec<u32> {
        vec![0u32; w * h]
    }

    #[test]
    fn paint_fills_strip_with_bg_started() {
        let w = 800;
        let h = DOWNLOAD_NOTICE_HEIGHT as usize;
        let mut buf = make_buf(w, h);
        let s = DownloadNoticeStrip {
            kind: DownloadNoticeKind::Started,
            filename: "file.zip".into(),
            path: "/tmp/file.zip".into(),
        };
        s.paint(&mut buf, w, h, 0);
        // Far-right pixel of the last row — should be the bg colour.
        let idx = (h - 1) * w + (w - 1);
        assert_eq!(buf[idx], COLOUR_BG_STARTED);
    }

    #[test]
    fn paint_draws_accent_border_at_top() {
        let w = 800;
        let h = DOWNLOAD_NOTICE_HEIGHT as usize;
        let mut buf = make_buf(w, h);
        let s = DownloadNoticeStrip {
            kind: DownloadNoticeKind::Completed,
            filename: "file.zip".into(),
            path: "/tmp/file.zip".into(),
        };
        s.paint(&mut buf, w, h, 0);
        // First row, mid-column → accent border.
        assert_eq!(buf[w / 2], COLOUR_ACCENT_COMPLETED);
    }

    #[test]
    fn paint_failed_uses_red_accent() {
        let w = 800;
        let h = DOWNLOAD_NOTICE_HEIGHT as usize;
        let mut buf = make_buf(w, h);
        let s = DownloadNoticeStrip {
            kind: DownloadNoticeKind::Failed,
            filename: "bad.zip".into(),
            path: String::new(),
        };
        s.paint(&mut buf, w, h, 0);
        assert_eq!(buf[w / 2], COLOUR_ACCENT_FAILED);
    }

    #[test]
    fn paint_strip_rows_are_nonzero() {
        let w = 400;
        let h = DOWNLOAD_NOTICE_HEIGHT as usize + 20;
        let mut buf = make_buf(w, h);
        let s = DownloadNoticeStrip {
            kind: DownloadNoticeKind::Completed,
            filename: "x.tar.gz".into(),
            path: "/tmp/x.tar.gz".into(),
        };
        s.paint(&mut buf, w, h, 0);
        // The strip rows should contain at least one painted pixel.
        let strip_has_paint = buf[..DOWNLOAD_NOTICE_HEIGHT as usize * w]
            .iter()
            .any(|&px| px != 0);
        assert!(strip_has_paint, "strip rows should not all be zero");
        // Rows beyond DOWNLOAD_NOTICE_HEIGHT must be untouched.
        let tail_clean = buf[DOWNLOAD_NOTICE_HEIGHT as usize * w..]
            .iter()
            .all(|&px| px == 0);
        assert!(tail_clean, "rows after the strip must remain zero");
    }

    #[test]
    fn paint_skips_when_top_y_off_screen() {
        let w = 200;
        let h = 28;
        let mut buf = make_buf(w, h);
        let s = DownloadNoticeStrip {
            kind: DownloadNoticeKind::Started,
            filename: "x".into(),
            path: String::new(),
        };
        // top_y past end of buffer → nothing written.
        s.paint(&mut buf, w, h, 1000);
        assert!(buf.iter().all(|&px| px == 0));
    }

    #[test]
    fn kind_variants_produce_distinct_accent_colours() {
        let (_, a_started) = palette(DownloadNoticeKind::Started);
        let (_, a_completed) = palette(DownloadNoticeKind::Completed);
        let (_, a_failed) = palette(DownloadNoticeKind::Failed);
        assert_ne!(a_started, a_completed);
        assert_ne!(a_started, a_failed);
        assert_ne!(a_completed, a_failed);
    }

    #[test]
    fn format_line_started_with_path() {
        let line = format_line(DownloadNoticeKind::Started, "file.zip", "/tmp/file.zip");
        assert!(line.starts_with("v "));
        assert!(line.contains("file.zip"));
        assert!(line.contains("/tmp/file.zip"));
    }

    #[test]
    fn format_line_completed() {
        let line = format_line(DownloadNoticeKind::Completed, "file.zip", "/dl/file.zip");
        assert!(line.starts_with("OK "));
    }

    #[test]
    fn format_line_failed_omits_path() {
        let line = format_line(DownloadNoticeKind::Failed, "bad.zip", "");
        assert!(line.starts_with("X "));
        assert!(!line.contains("->"));
    }
}
