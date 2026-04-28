//! Permissions-prompt overlay widget.
//!
//! Painted into the same softbuffer used by `Statusline` / `InputBar` /
//! `TabStrip`. Floats as a centered popup over the CEF region; the
//! caller draws the border and background and then calls `paint_at`.
//!
//! The widget is purely render-time. Decisions, queueing, and CEF
//! callback dispatch live in `apps/buffr` — this struct just describes
//! "what to paint right now".
//!
//! # Layout (inside the popup inner rect)
//!
//! ```text
//! +---------------------------------------------------------------+
//! | <origin> wants: camera, microphone     (2 more pending)       |
//! | [a]llow [d]eny [A]llow always [D]eny always [Esc]defer        |
//! +---------------------------------------------------------------+
//! ```

use crate::font;

/// Content height in pixels. Two text rows with padding.
pub const PERMISSIONS_PROMPT_HEIGHT: u32 = 60;

/// Render input for [`PermissionsPrompt::paint_at`]. Mirrors the data the
/// buffr permissions queue exposes; `Capability` is decoupled from the
/// `buffr-permissions` crate to keep `buffr-ui` from picking up a
/// rusqlite dep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionsPrompt {
    /// Display origin — typically the requesting URL's origin (scheme +
    /// host + port). Truncated from the right at paint time when too
    /// long for the content width.
    pub origin: String,
    /// Human-readable capability labels (e.g. "camera", "microphone").
    /// The widget joins them with `, ` for the action line.
    pub capabilities: Vec<String>,
    /// How many more requests are queued behind this one. `0` hides the
    /// `(N more pending)` indicator.
    pub queue_len: u32,
}

impl PermissionsPrompt {
    /// Paint the prompt content into the inner popup rect
    /// `(content_x, content_y, content_w, PERMISSIONS_PROMPT_HEIGHT)`.
    /// The caller is responsible for drawing the popup border and
    /// background before calling this. Returns `PERMISSIONS_PROMPT_HEIGHT`.
    pub fn paint_at(
        &self,
        buffer: &mut [u32],
        width: usize,
        height: usize,
        content_x: u32,
        content_y: u32,
        content_w: u32,
    ) -> u32 {
        if width == 0 || height == 0 || content_w == 0 {
            return PERMISSIONS_PROMPT_HEIGHT;
        }
        if buffer.len() < width * height {
            return PERMISSIONS_PROMPT_HEIGHT;
        }
        let top = content_y as i32;
        if top >= height as i32 {
            return PERMISSIONS_PROMPT_HEIGHT;
        }

        // Two text rows.
        let text_x: i32 = content_x as i32 + 8;
        let text_y0 = top + 8;
        let text_y1 = top + 8 + (font::glyph_h() as i32 + 8);

        let caps_joined = self.capabilities.join(", ");
        let line1 = if caps_joined.is_empty() {
            format!("{} wants permission", self.origin)
        } else {
            format!("{} wants: {caps_joined}", self.origin)
        };

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
        let right_edge = (content_x + content_w) as i32;
        let line1_max_px = if queue_text.is_some() {
            (right_edge - text_x - queue_w - right_pad - 12).max(0) as usize
        } else {
            (right_edge - text_x - right_pad).max(0) as usize
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
            let qx = right_edge - right_pad - queue_w;
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

        font::draw_text(
            buffer,
            width,
            height,
            text_x,
            text_y1,
            ACTION_HINT,
            COLOUR_PROMPT_HINT,
        );

        PERMISSIONS_PROMPT_HEIGHT
    }
}

/// Action hint string. `[a]` = allow once, `[d]` = deny once,
/// `[A]` = allow always, `[D]` = deny always, `[Esc]` = defer.
pub const ACTION_HINT: &str = "[a]llow [d]eny [A]llow always [D]eny always [Esc]defer";

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
    fn paint_at_does_not_panic_normal_case() {
        let w = 800;
        let h = 400;
        let mut buf = make_buf(w, h);
        let p = PermissionsPrompt {
            origin: "https://example.com".into(),
            capabilities: vec!["camera".into()],
            queue_len: 0,
        };
        let ret = p.paint_at(&mut buf, w, h, 100, 100, 600);
        assert_eq!(ret, PERMISSIONS_PROMPT_HEIGHT);
        // At least something was written inside the rect.
        assert!(buf.iter().any(|&px| px != 0));
    }

    #[test]
    fn paint_at_queue_and_no_queue_differ() {
        let w = 800;
        let h = 400;
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
        zero.paint_at(&mut buf_zero, w, h, 100, 100, 600);
        nonzero.paint_at(&mut buf_nonzero, w, h, 100, 100, 600);
        assert_ne!(buf_zero, buf_nonzero);
    }

    #[test]
    fn paint_at_skips_when_top_y_off_screen() {
        let w = 200;
        let h = 60;
        let mut buf = make_buf(w, h);
        let p = PermissionsPrompt {
            origin: "x".into(),
            capabilities: vec![],
            queue_len: 0,
        };
        p.paint_at(&mut buf, w, h, 0, 1000, 200);
        assert!(buf.iter().all(|&px| px == 0));
    }

    #[test]
    fn paint_at_handles_empty_capabilities_list() {
        let w = 600;
        let h = 400;
        let mut buf = make_buf(w, h);
        let p = PermissionsPrompt {
            origin: "https://x".into(),
            capabilities: vec![],
            queue_len: 0,
        };
        let ret = p.paint_at(&mut buf, w, h, 0, 0, 600);
        assert_eq!(ret, PERMISSIONS_PROMPT_HEIGHT);
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
