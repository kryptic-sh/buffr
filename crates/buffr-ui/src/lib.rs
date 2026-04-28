//! Browser chrome — Phase 3 statusline (the rest is deferred).
//!
//! See `docs/ui-stack.md` for the rendering decision: chrome lives in
//! a `softbuffer` strip docked to the bottom of the buffr window, in
//! the same `winit` window as the CEF child window above it.
//!
//! This crate stays free of `winit` and `softbuffer` types in its
//! public API — callers (`apps/buffr`) own surface lifecycle and pass
//! us a raw `&mut [u32]` slice each frame. That keeps the unit tests
//! trivial (build a `Vec<u32>`, paint, assert pixels) and avoids
//! coupling chrome rendering to any one window backend.

use buffr_modal::PageMode;

pub mod confirm_prompt;
pub mod download_notice;
pub mod font;
pub mod input_bar;
pub mod permissions_prompt;
pub mod tab_strip;

pub use confirm_prompt::{CONFIRM_PROMPT_HEIGHT, ConfirmPrompt, ConfirmRect, rect_contains};
pub use download_notice::{DOWNLOAD_NOTICE_HEIGHT, DownloadNoticeKind, DownloadNoticeStrip};
pub use input_bar::{
    INPUT_HEIGHT, InputBar, MAX_SUGGESTIONS, Palette as InputPalette, SUGGESTION_ROW_HEIGHT,
    Suggestion, SuggestionKind,
};
pub use permissions_prompt::{ACTION_HINT, PERMISSIONS_PROMPT_HEIGHT, PermissionsPrompt};
pub use tab_strip::{MAX_TAB_WIDTH, MIN_TAB_WIDTH, TAB_STRIP_HEIGHT, TabStrip, TabView};

/// Statusline strip height in pixels. 24 px fits a 10-px glyph row
/// with comfortable padding above + below; matches the recommendation
/// in `docs/ui-stack.md`. Bumping this requires the host window to
/// re-layout the CEF child rect.
pub const STATUSLINE_HEIGHT: u32 = 24;

/// Public re-export so embedders can pull `Mode` from one place.
pub use buffr_modal::Mode;

/// Coarse certificate state for the URL display. Phase 3 wires only
/// `Unknown`; CEF cert plumbing lands later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CertState {
    Secure,
    Insecure,
    Unknown,
}

/// Find-in-page status. Mirrors what CEF's `OnFindResult` callback
/// hands us, projected into a pair of `u32`s for the right-hand
/// statusline cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindStatus {
    pub query: String,
    pub current: u32,
    pub total: u32,
}

/// Update channel indicator surfaced in the right-hand statusline cell.
/// Mirrors `buffr_core::UpdateStatus` projected onto two render modes
/// (`* upd` for `Available`, `* upd?` for `Stale`). `None` hides the
/// cell entirely; the chrome doesn't paint a placeholder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateIndicator {
    /// Newer release available — `* upd`.
    Available,
    /// Cache is older than `check_interval_hours` — `* upd?`.
    Stale,
}

/// Snapshot of hint mode state. Rendered next to the cert pip when a
/// hint session is active. Mirrors `buffr_core::host::HintStatus` —
/// the indirection exists so `buffr-ui` doesn't pull `buffr-core` as a
/// dependency (would create a cycle).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HintStatus {
    pub typed: String,
    pub match_count: u32,
    pub background: bool,
}

/// Rendering input for the statusline. Re-create per frame; the
/// widget owns nothing.
#[derive(Debug, Clone)]
pub struct Statusline {
    pub mode: PageMode,
    pub url: String,
    /// Page load progress 0.0..=1.0. Drawn as a thin progress bar at
    /// the very top edge of the strip. Phase 3 doesn't yet wire CEF's
    /// `OnLoadingProgressChange` — keep at 1.0 until that lands.
    pub progress: f32,
    pub cert_state: CertState,
    /// Pending count buffer from the engine. `Some(0)` is treated as
    /// "no count"; the modal engine never emits a literal zero count
    /// here.
    pub count_buffer: Option<u32>,
    pub private: bool,
    pub find_query: Option<FindStatus>,
    /// Hint mode indicator. `Some(...)` while a hint session is live.
    pub hint_state: Option<HintStatus>,
    /// Phase 6 update channel indicator. `Some(...)` flags the user
    /// that an update is available (`* upd`) or that the cache is
    /// stale (`* upd?`). Click/tap doesn't go anywhere yet — the user
    /// runs `buffr --check-for-updates` manually.
    pub update_indicator: Option<UpdateIndicator>,
    /// Phase 6 a11y: when `true`, the strip uses the high-contrast
    /// palette (white on black) instead of the accent-tinted defaults.
    pub high_contrast: bool,
    /// Active tab's CEF zoom level. 0.0 is the page default; positive
    /// values zoom in, negative out. Rendered as a percentage in the
    /// statusline ("125%"). Hidden when at default.
    pub zoom_level: f64,
}

impl Default for Statusline {
    fn default() -> Self {
        Self {
            mode: PageMode::Normal,
            url: String::new(),
            progress: 1.0,
            cert_state: CertState::Unknown,
            count_buffer: None,
            private: false,
            find_query: None,
            hint_state: None,
            update_indicator: None,
            high_contrast: false,
            zoom_level: 0.0,
        }
    }
}

impl Statusline {
    /// Paint the statusline into the bottom `STATUSLINE_HEIGHT` rows
    /// of `buffer`. `buffer` is the *full* window buffer (one `u32`
    /// per pixel, row-major); we touch only the strip rows so the CEF
    /// child window above is undisturbed.
    ///
    /// `width` and `height` are the full window's pixel dimensions.
    /// If `height < STATUSLINE_HEIGHT` we draw nothing.
    pub fn paint(&self, buffer: &mut [u32], width: usize, height: usize) {
        let strip_h = STATUSLINE_HEIGHT as usize;
        if width == 0 || height < strip_h {
            return;
        }
        if buffer.len() < width * height {
            return;
        }

        // Strip starts at this row.
        let strip_y = height - strip_h;
        let bg = if self.high_contrast {
            HC_BG
        } else {
            mode_bg(self.mode)
        };
        let fg = if self.high_contrast {
            HC_FG
        } else {
            mode_fg(self.mode)
        };
        let accent = if self.high_contrast {
            HC_ACCENT
        } else {
            mode_accent(self.mode)
        };

        // Background fill — only the strip rows.
        fill_rect(buffer, width, height, 0, strip_y as i32, width, strip_h, bg);

        // Mode block — leftmost cell, slightly darker accent so the
        // text reads even when the rest of the strip shares the same
        // background colour.
        let mode_text = mode_label(self.mode);
        let mode_w = font::text_width(mode_text) + 12;
        fill_rect(
            buffer,
            width,
            height,
            0,
            strip_y as i32,
            mode_w,
            strip_h,
            accent,
        );
        // Vertically center: glyph height is 10, strip height is 24
        // → top padding (24 - 10) / 2 = 7.
        let text_y = strip_y as i32 + ((strip_h as i32 - font::GLYPH_H as i32) / 2);
        font::draw_text(buffer, width, height, 6, text_y, mode_text, fg);

        // Right-side cell: count buffer, find status, update channel,
        // and PRIVATE marker. Drawn right-to-left so each piece pads
        // naturally.
        let mut right_pen = width as i32 - 6;
        if self.private {
            let s = "PRIVATE";
            let w = font::text_width(s) as i32;
            right_pen -= w;
            let private_colour = if self.high_contrast {
                HC_FG
            } else {
                COLOUR_PRIVATE
            };
            font::draw_text(buffer, width, height, right_pen, text_y, s, private_colour);
            right_pen -= 8;
        }
        if let Some(ind) = self.update_indicator {
            let s = match ind {
                UpdateIndicator::Available => "* upd",
                UpdateIndicator::Stale => "* upd?",
            };
            let w = font::text_width(s) as i32;
            right_pen -= w;
            let upd_colour = if self.high_contrast {
                HC_FG
            } else {
                COLOUR_UPDATE
            };
            font::draw_text(buffer, width, height, right_pen, text_y, s, upd_colour);
            right_pen -= 8;
        }
        if let Some(find) = self.find_query.as_ref() {
            let s = format_find(find);
            let w = font::text_width(&s) as i32;
            right_pen -= w;
            font::draw_text(buffer, width, height, right_pen, text_y, &s, fg);
            right_pen -= 8;
        }
        if let Some(hint) = self.hint_state.as_ref() {
            let s = format_hint(hint);
            let w = font::text_width(&s) as i32;
            right_pen -= w;
            font::draw_text(buffer, width, height, right_pen, text_y, &s, fg);
            right_pen -= 8;
        }
        if let Some(count) = self.count_buffer
            && count > 0
        {
            let s = format!("{count}");
            let w = font::text_width(&s) as i32;
            right_pen -= w;
            font::draw_text(buffer, width, height, right_pen, text_y, &s, fg);
            right_pen -= 8;
        }
        // Zoom indicator. Hidden at default (0.0). CEF uses ~1.2^level
        // so 1 step ≈ 120%, -1 ≈ 83%. Round to nearest percent.
        if self.zoom_level.abs() > f64::EPSILON {
            let pct = (1.2_f64.powf(self.zoom_level) * 100.0).round() as i64;
            let s = format!("{pct}%");
            let w = font::text_width(&s) as i32;
            right_pen -= w;
            font::draw_text(buffer, width, height, right_pen, text_y, &s, fg);
            right_pen -= 8;
        }

        // URL middle cell. Truncate from the right if it would
        // overflow into the right-hand cell. Uses the cert state
        // colour as a left padlock byte.
        let url_x = mode_w as i32 + 6;
        let url_max_px = (right_pen - url_x).max(0) as usize;
        let url_text = truncate_to_width(&self.url, url_max_px);
        let cert_colour = match self.cert_state {
            CertState::Secure => COLOUR_CERT_SECURE,
            CertState::Insecure => COLOUR_CERT_INSECURE,
            CertState::Unknown => fg,
        };
        // Cert pip — single 2x6 vertical bar at the URL's left edge.
        fill_rect(
            buffer,
            width,
            height,
            url_x,
            strip_y as i32 + 8,
            2,
            font::GLYPH_H,
            cert_colour,
        );
        font::draw_text(buffer, width, height, url_x + 6, text_y, url_text, fg);

        // Progress bar — top 2 px of the strip. 0.0 = invisible,
        // 1.0 = full width. Phase 3 will animate this off CEF's
        // `OnLoadingProgressChange`.
        let progress = self.progress.clamp(0.0, 1.0);
        if progress > 0.0 && progress < 1.0 {
            let bar_w = (width as f32 * progress) as usize;
            fill_rect(
                buffer,
                width,
                height,
                0,
                strip_y as i32,
                bar_w,
                2,
                COLOUR_PROGRESS,
            );
        }
    }
}

fn format_hint(h: &HintStatus) -> String {
    let prefix = if h.background { "F" } else { "f" };
    if h.typed.is_empty() {
        format!("{prefix}: {} hints", h.match_count)
    } else {
        format!(
            "{prefix}: {} ({}/{})",
            h.typed,
            h.match_count,
            h.match_count.max(1)
        )
    }
}

fn format_find(f: &FindStatus) -> String {
    if f.total == 0 {
        format!("/{}: no matches", f.query)
    } else {
        format!("/{} {}/{}", f.query, f.current, f.total)
    }
}

/// Truncate `s` to at most `max_px` pixels of rendered width. Adds a
/// trailing `..` ellipsis when the original didn't fit.
pub(crate) fn truncate_to_width(s: &str, max_px: usize) -> &str {
    if font::text_width(s) <= max_px {
        return s;
    }
    if max_px < font::text_width("..") {
        return "";
    }
    // Walk backwards by char until the prefix + ".." fits.
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

#[allow(clippy::too_many_arguments)]
pub(crate) fn fill_rect(
    buffer: &mut [u32],
    width: usize,
    height: usize,
    x: i32,
    y: i32,
    w: usize,
    h: usize,
    colour: u32,
) {
    let x0 = x.max(0) as usize;
    let y0 = y.max(0) as usize;
    let x1 = (x.saturating_add(w as i32)).max(0) as usize;
    let y1 = (y.saturating_add(h as i32)).max(0) as usize;
    let x1 = x1.min(width);
    let y1 = y1.min(height);
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    for row in y0..y1 {
        let start = row * width + x0;
        let end = row * width + x1;
        if let Some(slice) = buffer.get_mut(start..end) {
            for pixel in slice {
                *pixel = colour;
            }
        }
    }
}

/// Mode label rendered into the leftmost statusline cell. Matches the
/// strings used by `apps/buffr` for the window title — keep these in
/// sync.
fn mode_label(mode: PageMode) -> &'static str {
    match mode {
        PageMode::Normal => "NORMAL",
        PageMode::Visual => "VISUAL",
        PageMode::Command => "COMMAND",
        PageMode::Hint => "HINT",
        PageMode::Insert => "INSERT",
        PageMode::Pending => "PENDING",
    }
}

// ---- colour table (RGB packed; alpha byte ignored) ------------------
//
// `softbuffer` 0.4 expects pixels as `0x00RRGGBB`; the high byte must
// be zero on Linux/X11. Picking saturated, vim-flavoured palette
// here — tweakable later via Phase 4 theme config.

const COLOUR_PROGRESS: u32 = 0x66_C2_FF;
const COLOUR_PRIVATE: u32 = 0xFF_C8_C8;
const COLOUR_CERT_SECURE: u32 = 0x66_E0_8A;
const COLOUR_CERT_INSECURE: u32 = 0xE0_5A_5A;
const COLOUR_UPDATE: u32 = 0xE0_C8_5A;

// Phase 6 high-contrast palette. Documented in `docs/accessibility.md`.
// Picked for WCAG-style contrast against the chrome's dark mode: pure
// white-on-black for body, a saturated yellow accent that survives
// black + white backgrounds, and a dimmed accent for secondary text.
//
// Colour values:
// - HC_BG:        0x000000  (pure black)
// - HC_FG:        0xFFFFFF  (pure white)
// - HC_ACCENT:    0xFFFF00  (high-contrast yellow)
// - HC_ACCENT_DIM:0xC0C0C0  (light grey, used for non-active accents)
pub const HC_BG: u32 = 0x00_00_00;
pub const HC_FG: u32 = 0xFF_FF_FF;
pub const HC_ACCENT: u32 = 0xFF_FF_00;
pub const HC_ACCENT_DIM: u32 = 0xC0_C0_C0;

const fn mode_bg(mode: PageMode) -> u32 {
    match mode {
        PageMode::Normal | PageMode::Pending => 0x16_30_18,
        PageMode::Visual => 0x33_22_06,
        PageMode::Command => 0x1A_1F_2E,
        PageMode::Hint => 0x2A_1A_2E,
        PageMode::Insert => 0x10_1F_30,
    }
}

const fn mode_accent(mode: PageMode) -> u32 {
    match mode {
        PageMode::Normal | PageMode::Pending => 0x4A_C9_5C,
        PageMode::Visual => 0xE0_8B_2A,
        PageMode::Command => 0x55_88_FF,
        PageMode::Hint => 0xC8_5A_E0,
        PageMode::Insert => 0x5A_AA_E0,
    }
}

const fn mode_fg(_mode: PageMode) -> u32 {
    0xEE_EE_EE
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_buf(w: usize, h: usize) -> Vec<u32> {
        vec![0u32; w * h]
    }

    #[test]
    fn paint_fills_strip_row_with_mode_bg() {
        let w = 200;
        let h = 24;
        let mut buf = make_buf(w, h);
        let s = Statusline {
            url: "https://example.com".into(),
            ..Statusline::default()
        };
        s.paint(&mut buf, w, h);
        // The leftmost column of the strip is owned by the mode-accent
        // cell — pixel (0,0) sits on the strip top row.
        assert_eq!(buf[0], mode_accent(PageMode::Normal));
    }

    #[test]
    fn paint_strip_pixel_outside_mode_block_uses_strip_bg() {
        let w = 400;
        let h = 24;
        let mut buf = make_buf(w, h);
        let s = Statusline {
            url: "x".into(),
            ..Statusline::default()
        };
        s.paint(&mut buf, w, h);
        // Far-right column on the bottom row — past mode block, past
        // URL text, almost certainly bg.
        let idx = (h - 1) * w + (w - 1);
        assert_eq!(buf[idx], mode_bg(PageMode::Normal));
    }

    #[test]
    fn paint_skips_when_height_less_than_strip() {
        let w = 100;
        let h = 10;
        let mut buf = make_buf(w, h);
        let s = Statusline::default();
        s.paint(&mut buf, w, h);
        // Buffer untouched — sentinel zero.
        assert!(buf.iter().all(|&p| p == 0));
    }

    #[test]
    fn mode_colours_differ() {
        // Smoke check that the palette assigns distinct accents per
        // mode — guards against future copy-paste regressions.
        let modes = [
            PageMode::Normal,
            PageMode::Visual,
            PageMode::Command,
            PageMode::Hint,
            PageMode::Insert,
        ];
        for (i, a) in modes.iter().enumerate() {
            for b in &modes[i + 1..] {
                assert_ne!(mode_accent(*a), mode_accent(*b), "{a:?} vs {b:?}");
            }
        }
    }

    #[test]
    fn truncate_to_width_short_string_unchanged() {
        let s = "hi";
        let max = 1000;
        assert_eq!(truncate_to_width(s, max), "hi");
    }

    #[test]
    fn truncate_to_width_returns_empty_when_too_narrow() {
        assert_eq!(truncate_to_width("abcd", 1), "");
    }

    #[test]
    fn truncate_to_width_drops_chars_until_fit() {
        // Width budget for "a" + ".." = 6 + 1 + (6+1+6) = 20 px.
        let dotdot = font::text_width("..");
        let one_a = font::text_width("a");
        let budget = one_a + dotdot;
        let s = "abcd";
        let out = truncate_to_width(s, budget);
        assert_eq!(out, "a");
    }

    #[test]
    fn format_find_no_matches() {
        let f = FindStatus {
            query: "foo".into(),
            current: 0,
            total: 0,
        };
        assert_eq!(format_find(&f), "/foo: no matches");
    }

    #[test]
    fn format_find_with_matches() {
        let f = FindStatus {
            query: "foo".into(),
            current: 2,
            total: 5,
        };
        assert_eq!(format_find(&f), "/foo 2/5");
    }

    #[test]
    fn format_hint_no_typed() {
        let h = HintStatus {
            typed: String::new(),
            match_count: 12,
            background: false,
        };
        assert_eq!(format_hint(&h), "f: 12 hints");
    }

    #[test]
    fn format_hint_with_typed_background() {
        let h = HintStatus {
            typed: "as".into(),
            match_count: 3,
            background: true,
        };
        assert!(format_hint(&h).starts_with("F:"));
        assert!(format_hint(&h).contains("as"));
    }

    #[test]
    fn high_contrast_uses_distinct_palette() {
        let w = 400;
        let h = 24;
        let mut buf_default = make_buf(w, h);
        let mut buf_hc = make_buf(w, h);
        let default_s = Statusline {
            url: "https://x".into(),
            ..Statusline::default()
        };
        let hc_s = Statusline {
            url: "https://x".into(),
            high_contrast: true,
            ..Statusline::default()
        };
        default_s.paint(&mut buf_default, w, h);
        hc_s.paint(&mut buf_hc, w, h);
        // Far-right pixel on the bottom row should differ — the
        // strip background paint sits there.
        let idx = (h - 1) * w + (w - 1);
        assert_ne!(buf_default[idx], buf_hc[idx]);
        // High-contrast strip background must be pure black.
        assert_eq!(buf_hc[idx], HC_BG);
    }

    #[test]
    fn high_contrast_palette_distinct_from_default_modes() {
        // Guard: HC values are not accidentally equal to any per-mode
        // default. (Catches a future palette refactor that picks the
        // same accent.)
        let modes = [
            PageMode::Normal,
            PageMode::Visual,
            PageMode::Command,
            PageMode::Hint,
            PageMode::Insert,
        ];
        for m in modes {
            assert_ne!(HC_ACCENT, mode_accent(m));
            assert_ne!(HC_BG, mode_bg(m));
        }
    }

    #[test]
    fn update_indicator_renders_when_set() {
        let w = 600;
        let h = 24;
        let mut buf_off = make_buf(w, h);
        let mut buf_on = make_buf(w, h);
        let off_s = Statusline {
            url: "x".into(),
            ..Statusline::default()
        };
        let on_s = Statusline {
            url: "x".into(),
            update_indicator: Some(UpdateIndicator::Available),
            ..Statusline::default()
        };
        off_s.paint(&mut buf_off, w, h);
        on_s.paint(&mut buf_on, w, h);
        assert_ne!(buf_off, buf_on);
    }

    #[test]
    fn private_marker_renders_distinctly() {
        let w = 400;
        let h = 24;
        let mut buf_priv = make_buf(w, h);
        let mut buf_norm = make_buf(w, h);
        let priv_s = Statusline {
            url: "https://x".into(),
            private: true,
            ..Statusline::default()
        };
        let norm_s = Statusline {
            url: "https://x".into(),
            private: false,
            ..Statusline::default()
        };
        priv_s.paint(&mut buf_priv, w, h);
        norm_s.paint(&mut buf_norm, w, h);
        assert_ne!(buf_priv, buf_norm);
    }
}
