//! Single-line text-input widget for the command line + omnibar.
//!
//! Phase 3 chrome: a 28-px-tall strip docked to the **top** of the
//! window, drawn with a softbuffer-style `&mut [u32]` blit. Same
//! protocol as the statusline at the bottom of the window — caller
//! owns surface lifecycle, we just paint pixels.
//!
//! # Layout
//!
//! ```text
//! +---------------------------------------------------+
//! | : | open https://example.com                     | <- input row (28px)
//! +---------------------------------------------------+
//! | open  open <url>                                  | <- suggestion 1 (24px)
//! | back  history back                                | <- suggestion 2 (24px)
//! | …                                                 |
//! +---------------------------------------------------+
//! ```
//!
//! # Cursor blink
//!
//! The widget exposes a [`InputBar::cursor_visible`] flag and the
//! caller is expected to flip it on a 500ms timer. We stay clock-free
//! so unit tests stay deterministic.
//!
//! # Glyph coverage
//!
//! The system font covers full Unicode; ASCII URLs work out of the box.
//! A bitmap fallback activates when no suitable system font is found.

use crate::font;
use crate::{STATUSLINE_HEIGHT, fill_rect};

/// Input strip height in pixels. Slightly taller than the statusline
/// so it reads as a separate UI affordance — and so the glyph row
/// has room for a 1-px focus border above and below.
pub const INPUT_HEIGHT: u32 = 28;

/// Suggestion row height. Matches the statusline so a stack of rows
/// reads as one cohesive overlay.
pub const SUGGESTION_ROW_HEIGHT: u32 = STATUSLINE_HEIGHT;

/// Maximum suggestions rendered. Past this, callers should truncate
/// their result set to avoid spilling off-screen.
pub const MAX_SUGGESTIONS: usize = 8;

/// What kind of suggestion is being rendered. Drives both the badge
/// colour and the relative ordering (history < bookmark < command <
/// search-engine fallback in the omnibar).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuggestionKind {
    History,
    Bookmark,
    Command,
    /// Synthesised "search the web for {query}" entry. Always the
    /// last row when present.
    SearchSuggestion,
}

/// One suggestion row. `display` is what's shown to the user;
/// `value` is what gets substituted into the buffer when the user
/// confirms a selection (Enter or Tab).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub display: String,
    pub value: String,
    pub kind: SuggestionKind,
}

/// Colour palette for the input bar. Mirrors the statusline palette
/// shape so a future theme system can wire both at once.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub bg: u32,
    pub fg: u32,
    pub accent: u32,
    pub border: u32,
    pub dropdown_bg: u32,
    pub dropdown_selected_bg: u32,
    pub dropdown_kind_history: u32,
    pub dropdown_kind_bookmark: u32,
    pub dropdown_kind_command: u32,
    pub dropdown_kind_search: u32,
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            bg: 0x1A_1F_2E,
            fg: 0xEE_EE_EE,
            accent: 0x55_88_FF,
            border: 0x33_3D_55,
            dropdown_bg: 0x14_18_24,
            dropdown_selected_bg: 0x22_2D_45,
            dropdown_kind_history: 0x88_AA_FF,
            dropdown_kind_bookmark: 0xE0_C8_5A,
            dropdown_kind_command: 0x4A_C9_5C,
            dropdown_kind_search: 0xC8_5A_E0,
        }
    }
}

/// Single-line text input + dropdown.
#[derive(Debug, Clone)]
pub struct InputBar {
    pub prefix: String,
    pub buffer: String,
    /// Byte index into [`Self::buffer`] where the next character would
    /// be inserted. `0..=buffer.len()` always; never points into the
    /// middle of a UTF-8 codepoint.
    pub cursor: usize,
    pub suggestions: Vec<Suggestion>,
    /// Index into [`Self::suggestions`]. `None` means "no selection,
    /// Enter uses the typed buffer verbatim".
    pub selected: Option<usize>,
    pub palette: Palette,
    /// Caller-managed cursor blink. Flip every 500ms or so.
    pub cursor_visible: bool,
}

impl Default for InputBar {
    fn default() -> Self {
        Self::with_prefix(":")
    }
}

impl InputBar {
    /// Build an empty input bar with the given prefix string.
    pub fn with_prefix(prefix: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into(),
            buffer: String::new(),
            cursor: 0,
            suggestions: Vec::new(),
            selected: None,
            palette: Palette::default(),
            cursor_visible: true,
        }
    }

    /// What the caller should treat as the "confirmed value" on Enter.
    /// If a suggestion is selected, returns its `value`; else returns
    /// the raw buffer.
    pub fn current_value(&self) -> &str {
        if let Some(idx) = self.selected
            && let Some(s) = self.suggestions.get(idx)
        {
            return s.value.as_str();
        }
        &self.buffer
    }

    /// Reset to "empty + prefix only" state. Doesn't clear the
    /// prefix itself — callers swap the widget for one with a new
    /// prefix when transitioning between command line / omnibar.
    pub fn clear(&mut self) {
        self.buffer.clear();
        self.cursor = 0;
        self.suggestions.clear();
        self.selected = None;
    }

    /// Insert `ch` at the cursor and advance.
    pub fn handle_text(&mut self, ch: char) {
        // Reject control characters; they're handled by dedicated
        // arrow / backspace methods.
        if ch.is_control() {
            return;
        }
        self.buffer.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
        // Editing always invalidates the suggestion selection; caller
        // re-runs `compute_suggestions` after this.
        self.selected = None;
    }

    /// Backspace — delete the codepoint before the cursor.
    pub fn handle_back(&mut self) {
        if self.cursor == 0 {
            return;
        }
        // Step back to the previous char boundary.
        let mut prev = self.cursor - 1;
        while !self.buffer.is_char_boundary(prev) && prev > 0 {
            prev -= 1;
        }
        self.buffer.replace_range(prev..self.cursor, "");
        self.cursor = prev;
        self.selected = None;
    }

    /// Delete the word before the cursor (`<C-w>`). A "word" is a run
    /// of non-whitespace, optionally preceded by whitespace.
    pub fn handle_delete_word(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let bytes = self.buffer.as_bytes();
        let mut end = self.cursor;
        // Skip trailing whitespace.
        while end > 0 && bytes[end - 1].is_ascii_whitespace() {
            end -= 1;
        }
        // Skip the word itself.
        while end > 0 && !bytes[end - 1].is_ascii_whitespace() {
            end -= 1;
        }
        self.buffer.replace_range(end..self.cursor, "");
        self.cursor = end;
        self.selected = None;
    }

    /// Clear the buffer entirely (`<C-u>`).
    pub fn handle_clear_line(&mut self) {
        self.buffer.clear();
        self.cursor = 0;
        self.selected = None;
    }

    /// Move cursor one codepoint left.
    pub fn handle_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let mut prev = self.cursor - 1;
        while !self.buffer.is_char_boundary(prev) && prev > 0 {
            prev -= 1;
        }
        self.cursor = prev;
    }

    /// Move cursor one codepoint right.
    pub fn handle_right(&mut self) {
        if self.cursor >= self.buffer.len() {
            return;
        }
        let mut next = self.cursor + 1;
        while next < self.buffer.len() && !self.buffer.is_char_boundary(next) {
            next += 1;
        }
        self.cursor = next;
    }

    /// Move suggestion selection one row up. Wraps from 0 → no-op.
    /// `<Up>` arrow / `<S-Tab>`.
    pub fn handle_up(&mut self) {
        if self.suggestions.is_empty() {
            self.selected = None;
            return;
        }
        self.selected = match self.selected {
            None => None,
            Some(0) => None,
            Some(n) => Some(n - 1),
        };
    }

    /// Move suggestion selection one row down. `<Down>` / `<Tab>`.
    pub fn handle_down(&mut self) {
        if self.suggestions.is_empty() {
            self.selected = None;
            return;
        }
        let max = self.suggestions.len() - 1;
        self.selected = match self.selected {
            None => Some(0),
            Some(n) if n >= max => Some(max),
            Some(n) => Some(n + 1),
        };
    }

    /// Replace the suggestion list. Resets `selected` to `None`.
    pub fn set_suggestions(&mut self, suggestions: Vec<Suggestion>) {
        self.suggestions = suggestions;
        if self.suggestions.len() > MAX_SUGGESTIONS {
            self.suggestions.truncate(MAX_SUGGESTIONS);
        }
        self.selected = None;
    }

    /// Total pixel height when fully expanded with `n` visible
    /// suggestions, where `n = min(suggestions.len(), MAX_SUGGESTIONS)`.
    /// Used by the host to compute the CEF child rect.
    pub fn total_height(&self) -> u32 {
        let rows = self.suggestions.len().min(MAX_SUGGESTIONS) as u32;
        INPUT_HEIGHT + rows * SUGGESTION_ROW_HEIGHT
    }

    /// Paint the input bar into the *top* `INPUT_HEIGHT` rows of
    /// `buffer`, then any visible suggestion rows below that. Rows
    /// below `total_height()` are left untouched — the caller has
    /// either reserved that space or has nothing else to draw there.
    pub fn paint(&self, buffer: &mut [u32], width: usize, height: usize) {
        self.paint_at(buffer, width, height, 0, 0, width, height);
    }

    /// Paint into a sub-rectangle of the surface buffer. `x`, `y`, `w`, `h`
    /// are pixel positions in the full surface (stride = `buf_w`). The bar
    /// draws at the top of the rect; suggestions extend downward within it.
    #[allow(clippy::too_many_arguments)]
    pub fn paint_at(
        &self,
        buffer: &mut [u32],
        buf_w: usize,
        buf_h: usize,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
    ) {
        if w == 0 || h < INPUT_HEIGHT as usize {
            return;
        }
        if buffer.len() < buf_w * buf_h {
            return;
        }

        let p = &self.palette;
        let bar_h = INPUT_HEIGHT as usize;

        // Background — fills the input row within the sub-rect.
        fill_rect(buffer, buf_w, buf_h, x as i32, y as i32, w, bar_h, p.bg);

        // Bottom 1-px border of the input row.
        fill_rect(
            buffer,
            buf_w,
            buf_h,
            x as i32,
            (y + bar_h) as i32 - 1,
            w,
            1,
            p.border,
        );

        let text_y = y as i32 + ((bar_h as i32) - font::glyph_h() as i32) / 2;

        // Prefix in accent.
        let prefix_x = x as i32 + 6;
        font::draw_text(
            buffer,
            buf_w,
            buf_h,
            prefix_x,
            text_y,
            &self.prefix,
            p.accent,
        );
        let prefix_w = font::text_width(&self.prefix) as i32;
        let buffer_x = prefix_x + prefix_w + 6;

        // Compute available pixel width for the buffer text and the
        // char-based scroll offset that keeps the cursor visible.
        let glyph_advance = font::glyph_w() + 1;
        let inner_w = (x as i32 + w as i32 - 6 - buffer_x).max(0) as usize;
        let chars_visible = (inner_w / glyph_advance).max(1);
        let cursor_chars = self.buffer[..self.cursor].chars().count();
        let total_chars = self.buffer.chars().count();
        let mut scroll_chars: usize = if cursor_chars >= chars_visible {
            cursor_chars + 1 - chars_visible
        } else {
            0
        };
        // Don't scroll past the end — keep the trailing edge of the
        // text within view when the buffer is shorter than scroll.
        let max_scroll = total_chars.saturating_sub(chars_visible.saturating_sub(1));
        if scroll_chars > max_scroll {
            scroll_chars = max_scroll;
        }
        // Visible substring.
        let visible: String = self
            .buffer
            .chars()
            .skip(scroll_chars)
            .take(chars_visible)
            .collect();
        font::draw_text(buffer, buf_w, buf_h, buffer_x, text_y, &visible, p.fg);

        // Cursor: 2-px-wide vertical bar at `cursor` char position
        // (relative to the scrolled substring).
        if self.cursor_visible && self.selected.is_none() {
            let cursor_offset = cursor_chars.saturating_sub(scroll_chars);
            let cursor_px = cursor_offset * glyph_advance;
            let cursor_x = buffer_x + cursor_px as i32;
            fill_rect(
                buffer,
                buf_w,
                buf_h,
                cursor_x,
                text_y - 1,
                2,
                font::glyph_h() + 2,
                p.fg,
            );
        }

        // Dropdown.
        if self.suggestions.is_empty() {
            return;
        }
        let row_h = SUGGESTION_ROW_HEIGHT as usize;
        for (i, sug) in self.suggestions.iter().take(MAX_SUGGESTIONS).enumerate() {
            let row_y = y + bar_h + i * row_h;
            if row_y + row_h > y + h {
                break;
            }
            let bg = if Some(i) == self.selected {
                p.dropdown_selected_bg
            } else {
                p.dropdown_bg
            };
            fill_rect(buffer, buf_w, buf_h, x as i32, row_y as i32, w, row_h, bg);
            // Kind pip.
            let pip_colour = match sug.kind {
                SuggestionKind::History => p.dropdown_kind_history,
                SuggestionKind::Bookmark => p.dropdown_kind_bookmark,
                SuggestionKind::Command => p.dropdown_kind_command,
                SuggestionKind::SearchSuggestion => p.dropdown_kind_search,
            };
            fill_rect(
                buffer,
                buf_w,
                buf_h,
                x as i32 + 6,
                row_y as i32 + 8,
                3,
                font::glyph_h(),
                pip_colour,
            );
            let row_text_y = row_y as i32 + ((row_h as i32 - font::glyph_h() as i32) / 2);
            let text_left = x + 16;
            let text_max_px = (x + w).saturating_sub(text_left + 8);
            let display = crate::truncate_to_width(&sug.display, text_max_px);
            font::draw_text(
                buffer,
                buf_w,
                buf_h,
                text_left as i32,
                row_text_y,
                display,
                p.fg,
            );
        }
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    fn s(d: &str) -> Suggestion {
        Suggestion {
            display: d.into(),
            value: d.into(),
            kind: SuggestionKind::History,
        }
    }

    #[test]
    fn handle_text_appends_at_cursor() {
        let mut b = InputBar::default();
        b.handle_text('h');
        b.handle_text('i');
        assert_eq!(b.buffer, "hi");
        assert_eq!(b.cursor, 2);
    }

    #[test]
    fn handle_text_inserts_in_middle() {
        let mut b = InputBar::default();
        b.buffer = "hi".into();
        b.cursor = 1;
        b.handle_text('e');
        assert_eq!(b.buffer, "hei");
        assert_eq!(b.cursor, 2);
    }

    #[test]
    fn handle_text_rejects_control_chars() {
        let mut b = InputBar::default();
        b.handle_text('\n');
        b.handle_text('\t');
        b.handle_text('\x07');
        assert_eq!(b.buffer, "");
    }

    #[test]
    fn handle_back_at_zero_is_noop() {
        let mut b = InputBar::default();
        b.handle_back();
        assert_eq!(b.buffer, "");
        assert_eq!(b.cursor, 0);
    }

    #[test]
    fn handle_back_deletes_codepoint() {
        let mut b = InputBar::default();
        b.buffer = "hi".into();
        b.cursor = 2;
        b.handle_back();
        assert_eq!(b.buffer, "h");
        assert_eq!(b.cursor, 1);
    }

    #[test]
    fn handle_delete_word_word_only() {
        let mut b = InputBar::default();
        b.buffer = "hello world".into();
        b.cursor = 11;
        b.handle_delete_word();
        assert_eq!(b.buffer, "hello ");
        assert_eq!(b.cursor, 6);
    }

    #[test]
    fn handle_delete_word_with_trailing_space() {
        let mut b = InputBar::default();
        b.buffer = "hello world  ".into();
        b.cursor = 13;
        b.handle_delete_word();
        assert_eq!(b.buffer, "hello ");
    }

    #[test]
    fn handle_clear_line_empties() {
        let mut b = InputBar::default();
        b.buffer = "stuff".into();
        b.cursor = 5;
        b.handle_clear_line();
        assert_eq!(b.buffer, "");
        assert_eq!(b.cursor, 0);
    }

    #[test]
    fn handle_left_clamps_at_zero() {
        let mut b = InputBar::default();
        b.handle_left();
        assert_eq!(b.cursor, 0);
        b.buffer = "hi".into();
        b.cursor = 1;
        b.handle_left();
        assert_eq!(b.cursor, 0);
        b.handle_left();
        assert_eq!(b.cursor, 0);
    }

    #[test]
    fn handle_right_clamps_at_end() {
        let mut b = InputBar::default();
        b.buffer = "hi".into();
        b.cursor = 0;
        b.handle_right();
        assert_eq!(b.cursor, 1);
        b.handle_right();
        assert_eq!(b.cursor, 2);
        b.handle_right();
        assert_eq!(b.cursor, 2);
    }

    #[test]
    fn up_down_clamp_at_boundaries() {
        let mut b = InputBar::default();
        b.set_suggestions(vec![s("a"), s("b"), s("c")]);
        // Initially no selection.
        assert_eq!(b.selected, None);
        b.handle_down();
        assert_eq!(b.selected, Some(0));
        b.handle_down();
        assert_eq!(b.selected, Some(1));
        b.handle_down();
        assert_eq!(b.selected, Some(2));
        b.handle_down(); // clamp
        assert_eq!(b.selected, Some(2));
        b.handle_up();
        assert_eq!(b.selected, Some(1));
        b.handle_up();
        assert_eq!(b.selected, Some(0));
        b.handle_up(); // back to none
        assert_eq!(b.selected, None);
        b.handle_up(); // clamp
        assert_eq!(b.selected, None);
    }

    #[test]
    fn current_value_uses_selection_when_set() {
        let mut b = InputBar::default();
        b.buffer = "typed".into();
        b.set_suggestions(vec![Suggestion {
            display: "first".into(),
            value: "first-value".into(),
            kind: SuggestionKind::History,
        }]);
        assert_eq!(b.current_value(), "typed");
        b.handle_down();
        assert_eq!(b.current_value(), "first-value");
    }

    #[test]
    fn current_value_falls_back_when_no_suggestions() {
        let mut b = InputBar::default();
        b.buffer = "raw".into();
        assert_eq!(b.current_value(), "raw");
    }

    #[test]
    fn set_suggestions_truncates_to_max() {
        let mut b = InputBar::default();
        let many: Vec<_> = (0..20).map(|i| s(&format!("row{i}"))).collect();
        b.set_suggestions(many);
        assert_eq!(b.suggestions.len(), MAX_SUGGESTIONS);
    }

    #[test]
    fn total_height_grows_with_suggestion_count() {
        let mut b = InputBar::default();
        assert_eq!(b.total_height(), INPUT_HEIGHT);
        b.set_suggestions(vec![s("a"), s("b")]);
        assert_eq!(b.total_height(), INPUT_HEIGHT + 2 * SUGGESTION_ROW_HEIGHT);
    }

    #[test]
    fn paint_smoke_no_crash_with_dropdown() {
        let w = 400;
        let h = 200;
        let mut buf = vec![0u32; w * h];
        let mut b = InputBar::default();
        b.buffer = "hello".into();
        b.cursor = 5;
        b.set_suggestions(vec![s("first"), s("second")]);
        b.handle_down();
        b.paint(&mut buf, w, h);
        // Input row painted with bar bg.
        assert_eq!(buf[0], b.palette.bg);
    }

    #[test]
    fn editing_resets_selection() {
        let mut b = InputBar::default();
        b.set_suggestions(vec![s("a"), s("b")]);
        b.handle_down();
        assert_eq!(b.selected, Some(0));
        b.handle_text('x');
        assert_eq!(b.selected, None);
    }

    #[test]
    fn clear_resets_state() {
        let mut b = InputBar::default();
        b.buffer = "stuff".into();
        b.cursor = 5;
        b.set_suggestions(vec![s("a")]);
        b.handle_down();
        b.clear();
        assert_eq!(b.buffer, "");
        assert_eq!(b.cursor, 0);
        assert_eq!(b.selected, None);
        assert!(b.suggestions.is_empty());
    }
}
