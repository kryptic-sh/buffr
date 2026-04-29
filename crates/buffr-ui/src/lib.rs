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
pub use tab_strip::{
    FAVICON_RENDER_SIZE, MAX_TAB_WIDTH, MIN_TAB_WIDTH, TAB_STRIP_HEIGHT, TabFavicon, TabStrip,
    TabView,
};

/// Statusline strip height in pixels. 30 px fits a 14-px glyph row
/// with comfortable padding above + below; matches the recommendation
/// in `docs/ui-stack.md`. Bumping this requires the host window to
/// re-layout the CEF child rect.
pub const STATUSLINE_HEIGHT: u32 = 30;

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
    /// Active tab's CEF zoom level. 0.0 is the page default; positive
    /// values zoom in, negative out. Rendered as a percentage in the
    /// statusline ("125%"). Hidden when at default.
    pub zoom_level: f64,
    /// Chrome colours. Set once on startup from `config.theme`; flip
    /// to [`Palette::high_contrast`] when `theme.high_contrast = true`.
    pub palette: Palette,
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
            zoom_level: 0.0,
            palette: Palette::default(),
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
        let p = &self.palette;
        let mode_bg = p.mode_bg(self.mode);
        let mode_accent = p.mode_accent(self.mode);

        // Background fill — only the strip rows. Tinted per-mode so
        // the whole strip (not just the leftmost cell) carries the
        // chromatic mode signal.
        fill_rect(
            buffer,
            width,
            height,
            0,
            strip_y as i32,
            width,
            strip_h,
            mode_bg,
        );

        // Mode block — leftmost cell, accent-on-dark so the label
        // pops against the rest of the strip.
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
            mode_accent,
        );
        let text_y = strip_y as i32 + ((strip_h as i32 - font::glyph_h() as i32) / 2);
        font::draw_text(buffer, width, height, 6, text_y, mode_text, mode_bg);

        // Right-side cell: count buffer, find status, update channel,
        // and PRIVATE marker. Drawn right-to-left so each piece pads
        // naturally.
        let mut right_pen = width as i32 - 6;
        if self.private {
            let s = "PRIVATE";
            let w = font::text_width(s) as i32;
            right_pen -= w;
            font::draw_text(buffer, width, height, right_pen, text_y, s, p.private);
            right_pen -= 8;
        }
        if let Some(ind) = self.update_indicator {
            let s = match ind {
                UpdateIndicator::Available => "* upd",
                UpdateIndicator::Stale => "* upd?",
            };
            let w = font::text_width(s) as i32;
            right_pen -= w;
            font::draw_text(buffer, width, height, right_pen, text_y, s, p.update);
            right_pen -= 8;
        }
        if let Some(find) = self.find_query.as_ref() {
            let s = format_find(find);
            let w = font::text_width(&s) as i32;
            right_pen -= w;
            font::draw_text(buffer, width, height, right_pen, text_y, &s, p.fg);
            right_pen -= 8;
        }
        if let Some(hint) = self.hint_state.as_ref() {
            let s = format_hint(hint);
            let w = font::text_width(&s) as i32;
            right_pen -= w;
            font::draw_text(buffer, width, height, right_pen, text_y, &s, p.fg);
            right_pen -= 8;
        }
        if let Some(count) = self.count_buffer
            && count > 0
        {
            let s = format!("{count}");
            let w = font::text_width(&s) as i32;
            right_pen -= w;
            font::draw_text(buffer, width, height, right_pen, text_y, &s, p.fg);
            right_pen -= 8;
        }
        // Zoom indicator. Hidden at default (0.0). CEF uses ~1.2^level
        // so 1 step ≈ 120%, -1 ≈ 83%. Round to nearest percent.
        if self.zoom_level.abs() > f64::EPSILON {
            let pct = (1.2_f64.powf(self.zoom_level) * 100.0).round() as i64;
            let s = format!("{pct}%");
            let w = font::text_width(&s) as i32;
            right_pen -= w;
            font::draw_text(buffer, width, height, right_pen, text_y, &s, p.fg);
            right_pen -= 8;
        }

        // URL middle cell. Truncate from the right if it would
        // overflow into the right-hand cell. Uses the cert state
        // colour as a left padlock byte.
        let url_x = mode_w as i32 + 6;
        let url_max_px = (right_pen - url_x).max(0) as usize;
        let url_text = truncate_to_width(&self.url, url_max_px);
        let cert_colour = match self.cert_state {
            CertState::Secure => p.cert_secure,
            CertState::Insecure => p.cert_insecure,
            CertState::Unknown => p.fg,
        };
        // Cert pip — single 2x6 vertical bar at the URL's left edge.
        fill_rect(
            buffer,
            width,
            height,
            url_x,
            strip_y as i32 + 8,
            2,
            font::glyph_h(),
            cert_colour,
        );
        font::draw_text(buffer, width, height, url_x + 6, text_y, url_text, p.fg);

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
                p.progress,
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

// ---- colour table (BGRA packed, alpha = 0xFF) -----------------------
//
// Pixels are u32 with layout `0xFF_RR_GG_BB` on little-endian: the byte
// sequence in memory is [B, G, R, A], matching `wgpu::TextureFormat::Bgra8Unorm`.
// The alpha byte is 0xFF (fully opaque) so the GPU alpha-blend pass
// composites chrome strips correctly over the OSR texture.
//
// Every chrome surface (statusline, tab strip, popup address bar) reads
// from a single [`Palette`], built once on startup from `config.theme`
// and re-derived when the theme reloads. Mode is communicated by the
// label glyph in the leftmost cell, not by colour — all five modes
// share the same accent so the chrome stays visually cohesive.

/// Single source of truth for chrome colours. Built from a base
/// accent plus a handful of semantic signals (cert state, private
/// marker, update indicator, progress bar). The non-accent fields
/// default to the historical fixed signals but are configurable via
/// `config.theme.*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    /// Base accent. Drives the mode block, active-tab indicator,
    /// and (darkened) the strip background.
    pub accent: u32,
    /// Strip / tab background — accent mixed heavily with black.
    pub bg: u32,
    /// Body text. Held at near-white for legibility regardless of
    /// accent hue; only the high-contrast palette overrides this.
    pub fg: u32,
    /// Inactive-tab background — slightly lifted from `bg` so tabs
    /// read as distinct without being visually loud.
    pub bg_lifted: u32,
    /// Inactive-tab foreground — dimmed `fg`.
    pub fg_dim: u32,
    /// Cert-state secure (lock icon, find counts).
    pub cert_secure: u32,
    /// Cert-state insecure.
    pub cert_insecure: u32,
    /// PRIVATE marker on the right-hand statusline cell.
    pub private: u32,
    /// Page-load progress bar.
    pub progress: u32,
    /// Update channel indicator (`* upd`).
    pub update: u32,
}

impl Palette {
    /// Derive a palette from a single base accent. `bg` is the accent
    /// mixed 92% with black; `bg_lifted` is the accent mixed 80%;
    /// `fg_dim` is `fg` mixed 35% with black. Semantic colours fall
    /// back to fixed signal values — callers override via
    /// [`Palette::with_signals`].
    pub fn from_accent(accent: u32) -> Self {
        Self {
            accent,
            bg: blend(accent, 0xFF_00_00_00, 0.92),
            fg: 0xFF_EE_EE_EE,
            bg_lifted: blend(accent, 0xFF_00_00_00, 0.85),
            fg_dim: 0xFF_A0_A8_AC,
            cert_secure: 0xFF_66_E0_8A,
            cert_insecure: 0xFF_E0_5A_5A,
            private: 0xFF_FF_C8_C8,
            progress: 0xFF_66_C2_FF,
            update: 0xFF_E0_C8_5A,
        }
    }

    /// Override the semantic-signal colours. Used when wiring
    /// `config.theme.{cert_secure,cert_insecure,private,progress,update}`.
    pub fn with_signals(
        mut self,
        cert_secure: u32,
        cert_insecure: u32,
        private: u32,
        progress: u32,
        update: u32,
    ) -> Self {
        self.cert_secure = cert_secure;
        self.cert_insecure = cert_insecure;
        self.private = private;
        self.progress = progress;
        self.update = update;
        self
    }

    /// Phase 6 high-contrast palette. Documented in `docs/accessibility.md`.
    /// Pure white-on-black + saturated yellow accent that survives both
    /// black and white backgrounds. Semantic signals collapse to white
    /// so the chrome stays legible for low-vision users.
    pub fn high_contrast() -> Self {
        Self {
            accent: 0xFF_FF_FF_00,
            bg: 0xFF_00_00_00,
            fg: 0xFF_FF_FF_FF,
            bg_lifted: 0xFF_10_10_10,
            fg_dim: 0xFF_C0_C0_C0,
            cert_secure: 0xFF_FF_FF_FF,
            cert_insecure: 0xFF_FF_FF_FF,
            private: 0xFF_FF_FF_FF,
            progress: 0xFF_FF_FF_FF,
            update: 0xFF_FF_FF_FF,
        }
    }
}

impl Default for Palette {
    fn default() -> Self {
        // Match the page accent on `https://buffr.kryptic.sh/`.
        Self::from_accent(0xFF_7A_A2_F7)
    }
}

/// Linear blend between two BGRA pixels. `t = 0.0` returns `a`,
/// `t = 1.0` returns `b`. Alpha is held at `0xFF`.
pub(crate) fn blend(a: u32, b: u32, t: f32) -> u32 {
    let extract = |c: u32, shift: u32| -> u8 { ((c >> shift) & 0xFF) as u8 };
    let lerp = |x: u8, y: u8| -> u8 {
        ((x as f32) * (1.0 - t) + (y as f32) * t)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    let r = lerp(extract(a, 16), extract(b, 16));
    let g = lerp(extract(a, 8), extract(b, 8));
    let bb = lerp(extract(a, 0), extract(b, 0));
    0xFF_00_00_00 | ((r as u32) << 16) | ((g as u32) << 8) | (bb as u32)
}

/// Hue-rotation per-mode offsets (degrees). Picked so the five modes
/// land on visually distinct hues regardless of where the base accent
/// sits on the colour wheel — Normal stays at the user's accent, the
/// others fan out across roughly 360°.
const HUE_NORMAL: f32 = 0.0;
const HUE_INSERT: f32 = -40.0;
const HUE_VISUAL: f32 = 180.0;
const HUE_COMMAND: f32 = 80.0;
const HUE_HINT: f32 = 240.0;

fn mode_hue_offset(mode: PageMode) -> f32 {
    match mode {
        PageMode::Normal | PageMode::Pending => HUE_NORMAL,
        PageMode::Insert => HUE_INSERT,
        PageMode::Visual => HUE_VISUAL,
        PageMode::Command => HUE_COMMAND,
        PageMode::Hint => HUE_HINT,
    }
}

impl Palette {
    /// Per-mode accent — base accent hue-rotated so each mode lands on
    /// a distinct colour. High-contrast palette returns its yellow
    /// accent across all modes (mode is signaled by the label glyph).
    pub fn mode_accent(&self, mode: PageMode) -> u32 {
        if *self == Self::high_contrast() {
            return self.accent;
        }
        rotate_hue(self.accent, mode_hue_offset(mode))
    }

    /// Per-mode strip background — mode accent darkened the same 92%
    /// as the base `bg` so mode-tinted backgrounds stay subtle.
    pub fn mode_bg(&self, mode: PageMode) -> u32 {
        if *self == Self::high_contrast() {
            return self.bg;
        }
        blend(self.mode_accent(mode), 0xFF_00_00_00, 0.92)
    }
}

/// Rotate the hue of a BGRA pixel by `degrees`. Saturation and
/// lightness are preserved, so the rotated colour reads as "the same
/// vibe, different hue" — which is what we want for per-mode chrome.
fn rotate_hue(c: u32, degrees: f32) -> u32 {
    let r = ((c >> 16) & 0xFF) as f32 / 255.0;
    let g = ((c >> 8) & 0xFF) as f32 / 255.0;
    let b = (c & 0xFF) as f32 / 255.0;
    let (h, s, l) = rgb_to_hsl(r, g, b);
    let h2 = (h + degrees).rem_euclid(360.0);
    let (r2, g2, b2) = hsl_to_rgb(h2, s, l);
    let to_byte = |v: f32| (v * 255.0).round().clamp(0.0, 255.0) as u32;
    0xFF_00_00_00 | (to_byte(r2) << 16) | (to_byte(g2) << 8) | to_byte(b2)
}

fn rgb_to_hsl(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    if (max - min).abs() < f32::EPSILON {
        return (0.0, 0.0, l);
    }
    let d = max - min;
    let s = if l > 0.5 {
        d / (2.0 - max - min)
    } else {
        d / (max + min)
    };
    let h = if (max - r).abs() < f32::EPSILON {
        ((g - b) / d) + if g < b { 6.0 } else { 0.0 }
    } else if (max - g).abs() < f32::EPSILON {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    (h * 60.0, s, l)
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (f32, f32, f32) {
    if s.abs() < f32::EPSILON {
        return (l, l, l);
    }
    let q = if l < 0.5 {
        l * (1.0 + s)
    } else {
        l + s - l * s
    };
    let p = 2.0 * l - q;
    let h = h / 360.0;
    let hue = |t: f32| {
        let t = t.rem_euclid(1.0);
        if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        }
    };
    (hue(h + 1.0 / 3.0), hue(h), hue(h - 1.0 / 3.0))
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
        let h = STATUSLINE_HEIGHT as usize;
        let mut buf = make_buf(w, h);
        let s = Statusline {
            url: "https://example.com".into(),
            ..Statusline::default()
        };
        s.paint(&mut buf, w, h);
        // The leftmost column of the strip is owned by the mode-accent
        // cell — pixel (0,0) sits on the strip top row. Alpha is 0xFF (opaque).
        assert_eq!(buf[0], Palette::default().mode_accent(PageMode::Normal));
    }

    #[test]
    fn paint_strip_pixel_outside_mode_block_uses_strip_bg() {
        let w = 400;
        let h = STATUSLINE_HEIGHT as usize;
        let mut buf = make_buf(w, h);
        let s = Statusline {
            url: "x".into(),
            ..Statusline::default()
        };
        s.paint(&mut buf, w, h);
        // Far-right column on the bottom row — past mode block, past
        // URL text, almost certainly bg.
        let idx = (h - 1) * w + (w - 1);
        assert_eq!(buf[idx], Palette::default().mode_bg(PageMode::Normal));
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
    fn mode_accents_pairwise_distinct() {
        // Hue rotation must produce a distinct accent for every mode
        // pair — guards against future copy-paste regressions on the
        // HUE_* offsets.
        let p = Palette::default();
        let modes = [
            PageMode::Normal,
            PageMode::Visual,
            PageMode::Command,
            PageMode::Hint,
            PageMode::Insert,
        ];
        for (i, a) in modes.iter().enumerate() {
            for b in &modes[i + 1..] {
                assert_ne!(p.mode_accent(*a), p.mode_accent(*b), "{a:?} vs {b:?}");
            }
        }
    }

    #[test]
    fn palette_from_accent_derives_dark_bg() {
        // The strip bg is the accent darkened ~92%, so it must be
        // strictly darker than the accent on every channel.
        let p = Palette::from_accent(0xFF_7A_A2_F7);
        let extract = |c: u32, shift: u32| (c >> shift) & 0xFF;
        for shift in [0, 8, 16] {
            assert!(
                extract(p.bg, shift) < extract(p.accent, shift),
                "bg channel {shift} not darker than accent"
            );
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
        let h = STATUSLINE_HEIGHT as usize;
        let mut buf_default = make_buf(w, h);
        let mut buf_hc = make_buf(w, h);
        let default_s = Statusline {
            url: "https://x".into(),
            ..Statusline::default()
        };
        let hc_s = Statusline {
            url: "https://x".into(),
            palette: Palette::high_contrast(),
            ..Statusline::default()
        };
        default_s.paint(&mut buf_default, w, h);
        hc_s.paint(&mut buf_hc, w, h);
        // Far-right pixel on the bottom row should differ — the
        // strip background paint sits there.
        let idx = (h - 1) * w + (w - 1);
        assert_ne!(buf_default[idx], buf_hc[idx]);
        // High-contrast strip background must be pure black.
        assert_eq!(buf_hc[idx], Palette::high_contrast().bg);
    }

    #[test]
    fn high_contrast_palette_distinct_from_default_accent() {
        let hc = Palette::high_contrast();
        let dflt = Palette::default();
        assert_ne!(hc.accent, dflt.accent);
        assert_ne!(hc.bg, dflt.bg);
    }

    #[test]
    fn update_indicator_renders_when_set() {
        let w = 600;
        let h = STATUSLINE_HEIGHT as usize;
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
        let h = STATUSLINE_HEIGHT as usize;
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
