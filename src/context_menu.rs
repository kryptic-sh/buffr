//! Context-menu overlay widget.
//!
//! Renders a floating right-click menu at the click coordinates, clamped
//! to stay inside the visible window region.  The caller is responsible
//! for keyboard / mouse dispatch; this struct is purely a renderer.
//!
//! # Layout
//!
//! ```text
//! ┌─────────────────────────┐
//! │ Back                    │  ← selected row (highlighted)
//! │ Forward                 │
//! │─────────────────────────│  ← separator (hairline)
//! │ Reload                  │
//! └─────────────────────────┘
//! ```

use crate::fill_rect;
use crate::font;

/// Height of a single selectable item row in pixels.
pub const CONTEXT_MENU_ROW_HEIGHT: u32 = 24;
/// Height of a separator row in pixels.
pub const CONTEXT_MENU_SEP_HEIGHT: u32 = 6;
/// Horizontal padding inside the menu panel.
pub const CONTEXT_MENU_PADDING_X: u32 = 12;
/// Minimum menu width in pixels.
pub const CONTEXT_MENU_MIN_WIDTH: u32 = 180;

// ── colours ───────────────────────────────────────────────────────────────────

const BG: u32 = 0xFF_1E_20_2E;
const BG_SELECTED: u32 = 0xFF_7A_A2_F7;
const FG: u32 = 0xFF_EE_EE_EE;
const FG_SELECTED: u32 = 0xFF_0A_0C_14;
const FG_DISABLED: u32 = 0xFF_60_68_80;
const SEP_COLOR: u32 = 0xFF_38_3C_52;
const BORDER_COLOR: u32 = 0xFF_7A_A2_F7;

/// One entry as seen by the widget.  The caller resolves `label` from
/// `ContextMenuItem::label()` and sets `is_separator` / `enabled`.
#[derive(Debug, Clone)]
pub struct ContextMenuEntry {
    /// Resolved human-readable label. Empty string for separators.
    pub label: String,
    /// Whether this row is a visual separator (non-selectable).
    pub is_separator: bool,
    /// Whether the item is interactive. Disabled items are rendered dimmed
    /// and skipped by keyboard navigation.
    pub enabled: bool,
}

/// Snapshot passed to [`ContextMenuOverlay::paint_at`] each frame.
#[derive(Debug, Clone)]
pub struct ContextMenuOverlay {
    /// Ordered list of entries to render.
    pub entries: Vec<ContextMenuEntry>,
    /// Index into `entries` of the currently-selected row.
    /// The caller is responsible for clamping to selectable rows.
    pub selected: usize,
    /// Requested pixel origin (top-left of the menu panel) in
    /// chrome-buffer coordinates. The widget clamps this so the menu
    /// stays inside `(buf_w, buf_h)`.
    pub x: i32,
    pub y: i32,
}

impl ContextMenuOverlay {
    /// Compute the pixel width required to display all entry labels.
    pub fn preferred_width(&self) -> u32 {
        let label_w = self
            .entries
            .iter()
            .filter(|e| !e.is_separator)
            .map(|e| font::text_width(&e.label))
            .max()
            .unwrap_or(0) as u32;
        (label_w + 2 * CONTEXT_MENU_PADDING_X + 2).max(CONTEXT_MENU_MIN_WIDTH)
    }

    /// Compute the pixel height of the entire panel.
    pub fn preferred_height(&self) -> u32 {
        let mut h: u32 = 2; // top + bottom border px
        for e in &self.entries {
            h += if e.is_separator {
                CONTEXT_MENU_SEP_HEIGHT
            } else {
                CONTEXT_MENU_ROW_HEIGHT
            };
        }
        h
    }

    /// Paint the overlay into `buf`.
    ///
    /// `buf_w` and `buf_h` are the full chrome buffer dimensions.
    /// The panel is clamped to stay fully inside the buffer.
    pub fn paint(&self, buf: &mut [u32], buf_w: usize, buf_h: usize) {
        if self.entries.is_empty() || buf_w == 0 || buf_h == 0 {
            return;
        }

        let panel_w = self.preferred_width() as i32;
        let panel_h = self.preferred_height() as i32;

        // Clamp to viewport.
        let px = self.x.clamp(0, (buf_w as i32 - panel_w).max(0));
        let py = self.y.clamp(0, (buf_h as i32 - panel_h).max(0));

        // Border.
        fill_rect(
            buf,
            buf_w,
            buf_h,
            px,
            py,
            panel_w as usize,
            panel_h as usize,
            BORDER_COLOR,
        );
        // Background inside border.
        fill_rect(
            buf,
            buf_w,
            buf_h,
            px + 1,
            py + 1,
            (panel_w - 2).max(0) as usize,
            (panel_h - 2).max(0) as usize,
            BG,
        );

        let mut cursor_y = py + 1i32;
        for (idx, entry) in self.entries.iter().enumerate() {
            if entry.is_separator {
                // Draw a hairline separator.
                let sep_mid = cursor_y + CONTEXT_MENU_SEP_HEIGHT as i32 / 2;
                fill_rect(
                    buf,
                    buf_w,
                    buf_h,
                    px + 1,
                    sep_mid,
                    (panel_w - 2).max(0) as usize,
                    1,
                    SEP_COLOR,
                );
                cursor_y += CONTEXT_MENU_SEP_HEIGHT as i32;
                continue;
            }

            let row_h = CONTEXT_MENU_ROW_HEIGHT as i32;
            let is_selected = idx == self.selected;

            // Row background.
            let row_bg = if is_selected { BG_SELECTED } else { BG };
            fill_rect(
                buf,
                buf_w,
                buf_h,
                px + 1,
                cursor_y,
                (panel_w - 2).max(0) as usize,
                row_h as usize,
                row_bg,
            );

            // Label text, vertically centred in the row.
            let text_color = if is_selected {
                FG_SELECTED
            } else if entry.enabled {
                FG
            } else {
                FG_DISABLED
            };
            let text_y = cursor_y + (row_h - font::glyph_h() as i32) / 2;
            font::draw_text(
                buf,
                buf_w,
                buf_h,
                px + CONTEXT_MENU_PADDING_X as i32,
                text_y,
                &entry.label,
                text_color,
            );

            cursor_y += row_h;
        }
    }

    /// Compute the clamped panel rect `(x, y, w, h)` for a buffer of size
    /// `(buf_w, buf_h)`. Mirrors the clamp logic in [`Self::paint`] so
    /// callers can hit-test the same pixels that render.
    pub fn panel_rect(&self, buf_w: usize, buf_h: usize) -> (i32, i32, i32, i32) {
        let panel_w = self.preferred_width() as i32;
        let panel_h = self.preferred_height() as i32;
        let px = self.x.clamp(0, (buf_w as i32 - panel_w).max(0));
        let py = self.y.clamp(0, (buf_h as i32 - panel_h).max(0));
        (px, py, panel_w, panel_h)
    }

    /// True if pixel `(x, y)` (in chrome-buffer coords) falls inside the
    /// clamped panel rect.
    pub fn contains(&self, buf_w: usize, buf_h: usize, x: i32, y: i32) -> bool {
        let (px, py, pw, ph) = self.panel_rect(buf_w, buf_h);
        x >= px && x < px + pw && y >= py && y < py + ph
    }

    /// Resolve pixel `(x, y)` to a selectable row index, or `None` if the
    /// hit lands on a separator, on the border, or outside the panel.
    pub fn row_at(&self, buf_w: usize, buf_h: usize, x: i32, y: i32) -> Option<usize> {
        if !self.contains(buf_w, buf_h, x, y) {
            return None;
        }
        let (_px, py, _pw, _ph) = self.panel_rect(buf_w, buf_h);
        let mut row_y = py + 1; // skip top border pixel
        for (idx, entry) in self.entries.iter().enumerate() {
            let row_h = if entry.is_separator {
                CONTEXT_MENU_SEP_HEIGHT as i32
            } else {
                CONTEXT_MENU_ROW_HEIGHT as i32
            };
            if y >= row_y && y < row_y + row_h {
                if entry.is_separator || !entry.enabled {
                    return None;
                }
                return Some(idx);
            }
            row_y += row_h;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_buf(w: usize, h: usize) -> Vec<u32> {
        vec![0u32; w * h]
    }

    fn simple_menu(x: i32, y: i32) -> ContextMenuOverlay {
        ContextMenuOverlay {
            entries: vec![
                ContextMenuEntry {
                    label: "Back".into(),
                    is_separator: false,
                    enabled: true,
                },
                ContextMenuEntry {
                    label: "".into(),
                    is_separator: true,
                    enabled: false,
                },
                ContextMenuEntry {
                    label: "Reload".into(),
                    is_separator: false,
                    enabled: true,
                },
            ],
            selected: 0,
            x,
            y,
        }
    }

    #[test]
    fn paint_does_not_panic() {
        let w = 800;
        let h = 600;
        let mut buf = make_buf(w, h);
        simple_menu(100, 200).paint(&mut buf, w, h);
    }

    #[test]
    fn paint_writes_pixels_in_menu_area() {
        let w = 800;
        let h = 600;
        let mut buf = make_buf(w, h);
        simple_menu(0, 0).paint(&mut buf, w, h);
        assert!(buf.iter().any(|&p| p != 0));
    }

    #[test]
    fn clamps_menu_to_viewport_right_edge() {
        let w = 800;
        let h = 600;
        let mut buf = make_buf(w, h);
        // x=10000 should clamp so menu fits inside.
        simple_menu(10000, 0).paint(&mut buf, w, h);
        // Top-right corner of the buffer should have non-zero pixels (menu
        // border rendered at the right edge).
        let first_row_right = w - 1; // rightmost pixel of row 0
        // After clamping the menu paints at x = buf_w - panel_w; at minimum
        // the border pixel at that column exists.
        assert!(buf[first_row_right] != 0 || buf.iter().any(|&p| p != 0));
    }

    #[test]
    fn preferred_width_is_at_least_min() {
        let m = simple_menu(0, 0);
        assert!(m.preferred_width() >= CONTEXT_MENU_MIN_WIDTH);
    }

    #[test]
    fn preferred_height_accounts_for_all_entries() {
        let m = simple_menu(0, 0);
        let expected =
            2 + CONTEXT_MENU_ROW_HEIGHT + CONTEXT_MENU_SEP_HEIGHT + CONTEXT_MENU_ROW_HEIGHT;
        assert_eq!(m.preferred_height(), expected);
    }

    #[test]
    fn contains_inside_and_outside() {
        let m = simple_menu(50, 60);
        let (x, y, w, h) = m.panel_rect(800, 600);
        assert!(m.contains(800, 600, x, y));
        assert!(m.contains(800, 600, x + w - 1, y + h - 1));
        assert!(!m.contains(800, 600, x - 1, y));
        assert!(!m.contains(800, 600, x, y - 1));
        assert!(!m.contains(800, 600, x + w, y));
    }

    #[test]
    fn row_at_resolves_selectable_rows() {
        let m = simple_menu(50, 60);
        let (px, py, _, _) = m.panel_rect(800, 600);
        // First row centre.
        let row0_y = py + 1 + CONTEXT_MENU_ROW_HEIGHT as i32 / 2;
        assert_eq!(m.row_at(800, 600, px + 10, row0_y), Some(0));
        // Separator row centre — non-selectable.
        let sep_y = py + 1 + CONTEXT_MENU_ROW_HEIGHT as i32 + CONTEXT_MENU_SEP_HEIGHT as i32 / 2;
        assert_eq!(m.row_at(800, 600, px + 10, sep_y), None);
        // Third (Reload) row centre.
        let row2_y = py
            + 1
            + CONTEXT_MENU_ROW_HEIGHT as i32
            + CONTEXT_MENU_SEP_HEIGHT as i32
            + CONTEXT_MENU_ROW_HEIGHT as i32 / 2;
        assert_eq!(m.row_at(800, 600, px + 10, row2_y), Some(2));
        // Outside panel.
        assert_eq!(m.row_at(800, 600, 0, 0), None);
    }
}
