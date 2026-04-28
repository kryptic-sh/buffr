//! Generic yes/no confirmation prompt.
//!
//! Painted into the same softbuffer as the rest of the chrome. Used
//! today for the "close pinned tab?" confirmation; structured so any
//! future yes/no decision can plug in by setting `message` and the
//! two button labels.
//!
//! Both buttons are clickable — the apps layer queries
//! [`ConfirmPrompt::button_rects_at`] to hit-test mouse events. Pressing
//! `y` / `n` (or `<Esc>` for No) is the keyboard equivalent and is
//! handled at the apps layer too.

use crate::{fill_rect, font};

/// Content height in pixels. The caller wraps this in a popup frame.
pub const CONFIRM_PROMPT_HEIGHT: u32 = 60;

/// Render input. Apps construct one of these when a confirmation is
/// pending and clear it once resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmPrompt {
    pub message: String,
    pub yes_label: String,
    pub no_label: String,
}

/// Pixel rect — `(x, y, w, h)`. Apps hit-test mouse clicks against
/// these to resolve the prompt.
pub type ConfirmRect = (i32, i32, i32, i32);

impl ConfirmPrompt {
    /// Pixel rects for the Yes and No buttons in window coordinates.
    /// `content_x/y` are the top-left of the inner popup rect;
    /// `content_w` is its width. Deterministic so the apps layer can
    /// hit-test without consulting paint-time state.
    pub fn button_rects_at(
        &self,
        content_x: u32,
        content_y: u32,
        content_w: u32,
    ) -> (ConfirmRect, ConfirmRect) {
        let right = (content_x + content_w) as i32;
        let btn_h = BUTTON_H;
        let btn_y = content_y as i32 + (CONFIRM_PROMPT_HEIGHT as i32 - btn_h) / 2;
        let yes_w = (font::text_width(&self.yes_label) as i32 + BUTTON_PAD_X * 2).max(40);
        let no_w = (font::text_width(&self.no_label) as i32 + BUTTON_PAD_X * 2).max(40);
        let gap = 12;
        let right_pad = 16;
        let no_x = right - right_pad - no_w;
        let yes_x = no_x - gap - yes_w;
        ((yes_x, btn_y, yes_w, btn_h), (no_x, btn_y, no_w, btn_h))
    }

    /// Paint the prompt content into the inner popup rect
    /// `(content_x, content_y, content_w, CONFIRM_PROMPT_HEIGHT)`.
    /// The caller is responsible for drawing the popup border and
    /// background before calling this. Returns `CONFIRM_PROMPT_HEIGHT`.
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
            return CONFIRM_PROMPT_HEIGHT;
        }
        if buffer.len() < width * height {
            return CONFIRM_PROMPT_HEIGHT;
        }
        let top = content_y as i32;
        if top >= height as i32 {
            return CONFIRM_PROMPT_HEIGHT;
        }

        let text_x = content_x as i32 + 8;
        let text_y = top + (CONFIRM_PROMPT_HEIGHT as i32 - font::GLYPH_H as i32) / 2;
        font::draw_text(
            buffer,
            width,
            height,
            text_x,
            text_y,
            &self.message,
            COLOUR_FG,
        );

        let (yes, no) = self.button_rects_at(content_x, content_y, content_w);
        paint_button(buffer, width, height, yes, &self.yes_label, COLOUR_BTN_YES);
        paint_button(buffer, width, height, no, &self.no_label, COLOUR_BTN_NO);

        CONFIRM_PROMPT_HEIGHT
    }
}

fn paint_button(
    buffer: &mut [u32],
    width: usize,
    height: usize,
    rect: ConfirmRect,
    label: &str,
    bg: u32,
) {
    let (x, y, w, h) = rect;
    if w <= 0 || h <= 0 {
        return;
    }
    fill_rect(buffer, width, height, x, y, w as usize, h as usize, bg);
    let label_w = font::text_width(label) as i32;
    let label_x = x + (w - label_w) / 2;
    let label_y = y + (h - font::GLYPH_H as i32) / 2;
    font::draw_text(buffer, width, height, label_x, label_y, label, COLOUR_FG);
}

/// True when `(px, py)` falls inside `rect`.
pub fn rect_contains(rect: ConfirmRect, px: i32, py: i32) -> bool {
    let (x, y, w, h) = rect;
    px >= x && px < x + w && py >= y && py < y + h
}

const BUTTON_H: i32 = 28;
const BUTTON_PAD_X: i32 = 14;

const COLOUR_FG: u32 = 0xF0_E8_D8;
const COLOUR_BTN_YES: u32 = 0x40_28_28;
const COLOUR_BTN_NO: u32 = 0x28_28_28;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn button_rects_no_to_right_of_yes() {
        let p = ConfirmPrompt {
            message: "Close pinned tab?".into(),
            yes_label: "Yes".into(),
            no_label: "No".into(),
        };
        let content_x = 100u32;
        let content_w = 600u32;
        let (yes, no) = p.button_rects_at(content_x, 100, content_w);
        assert!(yes.0 < no.0, "Yes should sit left of No");
        assert!(
            no.0 + no.2 <= (content_x + content_w) as i32,
            "No must fit inside content width"
        );
    }

    #[test]
    fn rect_contains_works() {
        let r = (10, 20, 50, 30);
        assert!(rect_contains(r, 12, 22));
        assert!(rect_contains(r, 59, 49));
        assert!(!rect_contains(r, 60, 22));
        assert!(!rect_contains(r, 12, 50));
    }
}
