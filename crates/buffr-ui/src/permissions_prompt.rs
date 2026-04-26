//! Permissions-prompt overlay widget.
//!
//! Painted into the same softbuffer used by `Statusline` / `InputBar` /
//! `TabStrip`. Lives between the input bar (when open) and the tab
//! strip; sized at [`PERMISSIONS_PROMPT_HEIGHT`] = 60 px.
//!
//! The widget is purely render-time. Decisions, queueing, and CEF
//! callback dispatch live in `apps/buffr` — this struct just describes
//! "what to paint right now".
//!
//! # Layout
//!
//! ```text
//! +---------------------------------------------------------------+
//! | <origin> wants: camera, microphone     (2 more pending)       |
//! | [a]llow [d]eny [A]llow always [D]eny always [Esc]defer        |
//! +---------------------------------------------------------------+
//! ```
//!
//! Two rows of text plus 2 px of accent border at the very top so the
//! strip doesn't blend into the chrome behind it. `(N more pending)`
//! is omitted when `queue_len == 0`.

use crate::{fill_rect, font};

/// Strip height in pixels. 2 px accent + 4 px top pad + two 10-px text
/// rows + 8 px gap + 4 px bottom pad = 38, rounded up to 60 for
/// breathing room and so the page area shifts by a multiple of the
/// status / tab strip heights.
pub const PERMISSIONS_PROMPT_HEIGHT: u32 = 60;

/// Render input for [`PermissionsPrompt::paint`]. Mirrors the data the
/// buffr permissions queue exposes; `Capability` is decoupled from the
/// `buffr-permissions` crate to keep `buffr-ui` from picking up a
/// rusqlite dep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionsPrompt {
    /// Display origin — typically the requesting URL's origin (scheme +
    /// host + port). Truncated from the right at paint time when too
    /// long for the strip.
    pub origin: String,
    /// Human-readable capability labels (e.g. "camera", "microphone").
    /// The widget joins them with `, ` for the action line.
    pub capabilities: Vec<String>,
    /// How many more requests are queued behind this one. `0` hides the
    /// `(N more pending)` indicator.
    pub queue_len: u32,
}

impl PermissionsPrompt {
    /// Paint the prompt into the top [`PERMISSIONS_PROMPT_HEIGHT`] rows
    /// of `buffer` starting at `top_y`. `buffer` is the *full* window
    /// buffer (one `u32` per pixel, row-major); we touch only the strip
    /// rows so the chrome above + below remains intact.
    ///
    /// `width` and `height` are the full window's pixel dimensions. If
    /// `top_y + PERMISSIONS_PROMPT_HEIGHT > height` we paint as much as
    /// fits and clip the rest.
    pub fn paint(&self, buffer: &mut [u32], width: usize, height: usize, top_y: u32) {
        let strip_h = PERMISSIONS_PROMPT_HEIGHT as usize;
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

        // Background fill.
        fill_rect(
            buffer,
            width,
            height,
            0,
            top,
            width,
            strip_h,
            COLOUR_PROMPT_BG,
        );
        // Accent border — top 2 px so the strip reads as "alert".
        fill_rect(
            buffer,
            width,
            height,
            0,
            top,
            width,
            ACCENT_BAR_PX,
            COLOUR_PROMPT_ACCENT,
        );

        // Two text rows. Row 1 starts 8 px down from the top accent;
        // row 2 sits another 18 px below for legibility.
        let text_x: i32 = 8;
        let text_y0 = top + 8;
        let text_y1 = top + 8 + (font::GLYPH_H as i32 + 8);

        // Row 1: "<origin> wants: <caps>"
        let caps_joined = self.capabilities.join(", ");
        let line1 = if caps_joined.is_empty() {
            format!("{} wants permission", self.origin)
        } else {
            format!("{} wants: {caps_joined}", self.origin)
        };

        // Right-aligned queue indicator (only when more pending).
        let queue_text = if self.queue_len > 0 {
            Some(format!("({} more pending)", self.queue_len))
        } else {
            None
        };
        let queue_w = queue_text
            .as_ref()
            .map(|s| font::text_width(s) as i32)
            .unwrap_or(0);
        let right_pad: i32 = 8;
        // Truncate `line1` so it doesn't run into the queue indicator.
        let line1_max_px = if queue_text.is_some() {
            (width as i32 - text_x - queue_w - right_pad - 12).max(0) as usize
        } else {
            (width as i32 - text_x - right_pad).max(0) as usize
        };
        let line1_truncated = truncate_to_width(&line1, line1_max_px);
        font::draw_text(
            buffer,
            width,
            height,
            text_x,
            text_y0,
            line1_truncated,
            COLOUR_PROMPT_FG,
        );

        if let Some(qtext) = queue_text {
            let qx = (width as i32) - right_pad - queue_w;
            font::draw_text(
                buffer,
                width,
                height,
                qx,
                text_y0,
                &qtext,
                COLOUR_PROMPT_PENDING,
            );
        }

        // Row 2: action hints.
        font::draw_text(
            buffer,
            width,
            height,
            text_x,
            text_y1,
            ACTION_HINT,
            COLOUR_PROMPT_HINT,
        );
    }
}

/// Action hint string. `[a]` = allow once, `[d]` = deny once,
/// `[A]` = allow always, `[D]` = deny always, `[Esc]` = defer.
pub const ACTION_HINT: &str = "[a]llow [d]eny [A]llow always [D]eny always [Esc]defer";

const ACCENT_BAR_PX: usize = 2;

/// Truncate `s` to at most `max_px` pixels of rendered width. Adds a
/// trailing `..` ellipsis when the original didn't fit. Mirrors the
/// helper in `lib.rs` — duplicated here to avoid a `pub(crate)` leak.
fn truncate_to_width(s: &str, max_px: usize) -> &str {
    if font::text_width(s) <= max_px {
        return s;
    }
    if max_px < font::text_width("..") {
        return "";
    }
    let mut end = s.len();
    while end > 0 {
        let prefix = &s[..end];
        if !s.is_char_boundary(end) {
            end -= 1;
            continue;
        }
        if font::text_width(prefix) + font::text_width("..") <= max_px {
            return prefix;
        }
        end -= 1;
    }
    ""
}

// Distinct palette so the prompt visually separates from the rest of
// the chrome. Saturated red border + warm fg.
const COLOUR_PROMPT_BG: u32 = 0x16_0E_0E;
const COLOUR_PROMPT_ACCENT: u32 = 0xE0_5A_5A;
const COLOUR_PROMPT_FG: u32 = 0xF0_E8_D8;
const COLOUR_PROMPT_HINT: u32 = 0xC8_C8_C0;
const COLOUR_PROMPT_PENDING: u32 = 0xFF_C8_8C;

#[cfg(test)]
mod tests {
    use super::*;

    fn make_buf(w: usize, h: usize) -> Vec<u32> {
        vec![0u32; w * h]
    }

    #[test]
    fn paint_fills_strip_with_bg() {
        let w = 800;
        let h = PERMISSIONS_PROMPT_HEIGHT as usize;
        let mut buf = make_buf(w, h);
        let p = PermissionsPrompt {
            origin: "https://example.com".into(),
            capabilities: vec!["camera".into()],
            queue_len: 0,
        };
        p.paint(&mut buf, w, h, 0);
        // Center of the strip, well below the accent and well to the
        // right of the text region: should be the bg colour.
        let row = h / 2;
        let col = w - 4;
        let pixel = buf[row * w + col];
        assert_eq!(pixel, COLOUR_PROMPT_BG);
    }

    #[test]
    fn paint_draws_accent_at_top() {
        let w = 800;
        let h = PERMISSIONS_PROMPT_HEIGHT as usize;
        let mut buf = make_buf(w, h);
        let p = PermissionsPrompt {
            origin: "x".into(),
            capabilities: vec![],
            queue_len: 0,
        };
        p.paint(&mut buf, w, h, 0);
        // First row, mid-column — the accent bar.
        assert_eq!(buf[w / 2], COLOUR_PROMPT_ACCENT);
    }

    #[test]
    fn paint_no_queue_marker_when_empty() {
        let w = 800;
        let h = PERMISSIONS_PROMPT_HEIGHT as usize;
        let mut buf_zero = make_buf(w, h);
        let mut buf_nonzero = make_buf(w, h);
        let zero = PermissionsPrompt {
            origin: "https://x".into(),
            capabilities: vec!["camera".into()],
            queue_len: 0,
        };
        let nonzero = PermissionsPrompt {
            origin: "https://x".into(),
            capabilities: vec!["camera".into()],
            queue_len: 3,
        };
        zero.paint(&mut buf_zero, w, h, 0);
        nonzero.paint(&mut buf_nonzero, w, h, 0);
        // The queue indicator paints into the right-hand region only
        // when queue_len > 0, so the two buffers must differ somewhere
        // in that band.
        assert_ne!(buf_zero, buf_nonzero);
    }

    #[test]
    fn paint_skips_when_top_y_off_screen() {
        let w = 200;
        let h = 60;
        let mut buf = make_buf(w, h);
        let p = PermissionsPrompt {
            origin: "x".into(),
            capabilities: vec![],
            queue_len: 0,
        };
        // top_y past the last row → no writes.
        p.paint(&mut buf, w, h, 1000);
        assert!(buf.iter().all(|&px| px == 0));
    }

    #[test]
    fn paint_handles_empty_capabilities_list() {
        // Smoke: shouldn't panic and should still write something.
        let w = 600;
        let h = PERMISSIONS_PROMPT_HEIGHT as usize;
        let mut buf = make_buf(w, h);
        let p = PermissionsPrompt {
            origin: "https://x".into(),
            capabilities: vec![],
            queue_len: 0,
        };
        p.paint(&mut buf, w, h, 0);
        // Accent bar must still be present.
        assert_eq!(buf[w / 2], COLOUR_PROMPT_ACCENT);
    }

    #[test]
    fn truncate_to_width_short_unchanged() {
        assert_eq!(truncate_to_width("ok", 1000), "ok");
    }

    #[test]
    fn truncate_to_width_zero_budget_returns_empty() {
        assert_eq!(truncate_to_width("anything", 1), "");
    }
}
