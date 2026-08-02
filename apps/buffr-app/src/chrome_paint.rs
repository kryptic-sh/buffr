//! Software painting of buffr's own chrome into a `u32` BGRA buffer.
//!
//! Everything the app draws itself -- popup address bars, the modal
//! panel for the omnibar and confirm prompts, the tab strip and status
//! line -- as opposed to page content, which CEF renders into its own
//! OSR buffer.
//!
//! [`ModalPanel`] is the single source of truth for modal geometry:
//! the painter and the click hit-tests once computed it independently
//! and disagreed on any HiDPI scale.

use buffr_engine::TabId;
use buffr_ui::{
    ContextMenuOverlay, DownloadNoticeKind, DownloadNoticeStrip, InputBar, PermissionsPrompt,
    STATUSLINE_HEIGHT, Statusline, TabStrip,
};

/// Paint a minimal popup chrome: one address-bar strip at the top.
pub(crate) fn paint_popup_chrome(buf: &mut [u32], w: usize, h: usize, url: &str, bar_h: u32) {
    use buffr_ui::font;
    let bar_h = bar_h as usize;
    if w == 0 || h < bar_h {
        return;
    }
    // Background — same shade as the Normal statusline.
    let bg: u32 = 0xFF_16_30_18;
    let fg: u32 = 0xFF_EE_EE_EE;
    for row in 0..bar_h {
        let start = row * w;
        let end = (start + w).min(buf.len());
        if let Some(slice) = buf.get_mut(start..end) {
            for pixel in slice {
                *pixel = bg;
            }
        }
    }
    // URL text, left-padded by 8 px, vertically centred in bar_h.
    let text_y = ((bar_h as i32 - font::glyph_h() as i32) / 2).max(0) as i32;
    font::draw_text(buf, w, h, 8, text_y, url, fg);
}

/// Physical height of a popup window's address-bar strip.
///
/// [`STATUSLINE_HEIGHT`] is a LOGICAL (DIP) constant — correct inside the
/// popup's logical-sized chrome buffer, wrong everywhere it meets physical
/// coordinates (the OSR `dst_rect`, the pointer offset). Mixing the two is
/// invisible at scale 1 and halves the address bar at scale 2 (M31).
///
/// Degenerate `scale <= 0.0` is clamped to 1.0, matching
/// [`physical_cursor_to_dip`].
pub(crate) fn popup_bar_h_physical(scale: f32) -> u32 {
    let scale = if scale <= 0.0 { 1.0 } else { scale };
    ((STATUSLINE_HEIGHT as f32) * scale).round() as u32
}

/// Omnibar / command-line popup geometry.
pub(crate) const OMNIBAR_POPUP_MAX_WIDTH: u32 = 800;
pub(crate) const OMNIBAR_POPUP_BORDER: u32 = 2;
pub(crate) const OMNIBAR_POPUP_BG: u32 = 0xFF_1A_1B_26;
pub(crate) const OMNIBAR_POPUP_BORDER_COLOR: u32 = 0xFF_7A_A2_F7;
/// Minimum logical width of the confirm / permissions modal.
pub(crate) const CONFIRM_POPUP_MIN_WIDTH: u32 = 300;
/// Minimum logical width of the omnibar / command / find popup.
pub(crate) const OMNIBAR_POPUP_MIN_WIDTH: u32 = 200;

/// Geometry of a centred modal popup, in **logical** (DIP) pixels.
///
/// Single source of truth for both the chrome painter and the click
/// hit-tests. Before this existed the two computed the rect independently
/// — the painter in logical space, the hit-tests from
/// `window.physical_size()` — so on any HiDPI scale the confirm buttons
/// were drawn in one place and tested in another (M30).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ModalPanel {
    /// Border-box origin / size.
    pub(crate) x: u32,
    pub(crate) y: u32,
    pub(crate) w: u32,
    pub(crate) h: u32,
    /// Content box (border-box inset by [`OMNIBAR_POPUP_BORDER`]).
    pub(crate) inner_x: u32,
    pub(crate) inner_y: u32,
    pub(crate) inner_w: u32,
    pub(crate) inner_h: u32,
}

impl ModalPanel {
    /// Lay out a modal `content_h` logical pixels tall, centred
    /// horizontally in a `win_w`-wide chrome buffer and pinned to the
    /// upper third of `win_h`.
    ///
    /// `min_w` is a *desired* floor: it is capped at `win_w` first, so a
    /// window narrower than the floor yields `w == win_w` instead of
    /// `w > win_w`. The old code clamped unconditionally and then did
    /// `win_w - popup_w`, which underflows below the floor — debug panic,
    /// release wrap to ~4.29e9 and a popup painted off-screen. Reachable
    /// at a 500 px window on scale 2 (`lwidth == 250`, floor 300) (M32).
    pub(crate) fn new(win_w: u32, win_h: u32, min_w: u32, content_h: u32) -> Self {
        let min_w = min_w.min(win_w);
        let max_w = OMNIBAR_POPUP_MAX_WIDTH.min(win_w).max(min_w);
        // u64 for the percentage so a pathological width can't overflow.
        let w = (((win_w as u64) * 60 / 100) as u32).clamp(min_w, max_w);
        let x = (win_w - w) / 2;
        let y = win_h / 3;
        let h = (content_h + 2 * OMNIBAR_POPUP_BORDER).min(win_h.saturating_sub(y));
        Self {
            x,
            y,
            w,
            h,
            inner_x: x + OMNIBAR_POPUP_BORDER,
            inner_y: y + OMNIBAR_POPUP_BORDER,
            inner_w: w.saturating_sub(2 * OMNIBAR_POPUP_BORDER),
            inner_h: h.saturating_sub(2 * OMNIBAR_POPUP_BORDER),
        }
    }

    /// The confirm / permissions modal.
    pub(crate) fn confirm(win_w: u32, win_h: u32) -> Self {
        Self::new(
            win_w,
            win_h,
            CONFIRM_POPUP_MIN_WIDTH,
            buffr_ui::CONFIRM_PROMPT_HEIGHT,
        )
    }

    /// The omnibar / command / find popup, `content_h` tall.
    pub(crate) fn omnibar(win_w: u32, win_h: u32, content_h: u32) -> Self {
        Self::new(win_w, win_h, OMNIBAR_POPUP_MIN_WIDTH, content_h)
    }
}

/// Hit-test the pinned-close confirmation buttons in **logical** space.
///
/// `lwidth` / `lheight` are the logical chrome dims and `(lx, ly)` the
/// cursor converted to DIPs. Returns `Some(true)` for Yes, `Some(false)`
/// for No, `None` when the click missed both.
pub(crate) fn hit_test_confirm_buttons(
    lwidth: u32,
    lheight: u32,
    lx: i32,
    ly: i32,
) -> Option<bool> {
    let panel = ModalPanel::confirm(lwidth, lheight);
    // Labels must match the paint site — `button_rects_at` measures them.
    let confirm = buffr_ui::ConfirmPrompt {
        message: String::new(),
        yes_label: "Yes (y)".to_string(),
        no_label: "No (n)".to_string(),
    };
    let (yes_rect, no_rect) = confirm.button_rects_at(panel.inner_x, panel.inner_y, panel.inner_w);
    if buffr_ui::rect_contains(yes_rect, lx, ly) {
        return Some(true);
    }
    if buffr_ui::rect_contains(no_rect, lx, ly) {
        return Some(false);
    }
    None
}

/// Fill a rectangle in a u32 pixel buffer with stride `buf_w`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn fill_rect_u32(
    buf: &mut [u32],
    buf_w: usize,
    buf_h: usize,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    color: u32,
) {
    let x1 = (x + w).min(buf_w);
    let y1 = (y + h).min(buf_h);
    if x >= x1 || y >= y1 {
        return;
    }
    for row in y..y1 {
        let base = row * buf_w;
        let row_end = base + buf_w;
        if row_end > buf.len() {
            break;
        }
        buf[base + x..base + x1].fill(color);
    }
}

/// Paint only the chrome strips (statusline, tab strip, popups, download
/// notice, overlay) into `buf`. The CEF region rows are never touched —
/// they must remain at `0x00_00_00_00` (transparent) so the OSR texture
/// shows through the alpha-blended chrome layer.
///
/// All colours written here use `0xFF_RR_GG_BB` (fully-opaque BGRA) so
/// the GPU alpha-blend composite produces crisp chrome-over-OSR output.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_chrome_strips(
    buf: &mut [u32],
    w: usize,
    height: u32,
    statusline: &Statusline,
    tab_strip: &TabStrip,
    tab_y: u32,
    notice_y: u32,
    current_notice: Option<&buffr_core::DownloadNotice>,
    confirm_close_pinned: Option<TabId>,
    permissions_prompt: Option<&PermissionsPrompt>,
    overlay_data: Option<&InputBar>,
    context_menu: Option<&ContextMenuOverlay>,
) {
    let h = height as usize;

    // Statusline — bottom strip.
    statusline.paint(buf, w, h);

    // Tab strip — between download notice and CEF region.
    tab_strip.paint(buf, w, h, tab_y);

    // Permissions prompt OR pinned-close confirmation.
    let win_w = w as u32;
    let has_prompt = confirm_close_pinned.is_some() || permissions_prompt.is_some();
    if has_prompt {
        // Shared with `hit_test_confirm_buttons` — see `ModalPanel`.
        let ModalPanel {
            x: popup_x,
            y: popup_y,
            w: popup_w,
            h: popup_h,
            inner_x,
            inner_y,
            inner_w,
            inner_h,
        } = ModalPanel::confirm(win_w, height);
        fill_rect_u32(
            buf,
            w,
            h,
            popup_x as usize,
            popup_y as usize,
            popup_w as usize,
            popup_h as usize,
            OMNIBAR_POPUP_BORDER_COLOR,
        );
        fill_rect_u32(
            buf,
            w,
            h,
            inner_x as usize,
            inner_y as usize,
            inner_w as usize,
            inner_h as usize,
            OMNIBAR_POPUP_BG,
        );
        if confirm_close_pinned.is_some() {
            let confirm_widget = buffr_ui::ConfirmPrompt {
                message: "Close pinned tab?".to_string(),
                yes_label: "Yes (y)".to_string(),
                no_label: "No (n)".to_string(),
            };
            confirm_widget.paint_at(buf, w, h, inner_x, inner_y, inner_w);
        } else if let Some(prompt) = permissions_prompt {
            prompt.paint_at(buf, w, h, inner_x, inner_y, inner_w);
        }
    }

    // Download notice strip.
    if let Some(notice) = current_notice {
        let strip = DownloadNoticeStrip {
            kind: match notice.kind {
                buffr_core::DownloadNoticeKind::Started => DownloadNoticeKind::Started,
                buffr_core::DownloadNoticeKind::Completed => DownloadNoticeKind::Completed,
                buffr_core::DownloadNoticeKind::Failed => DownloadNoticeKind::Failed,
            },
            filename: notice.filename.clone(),
            path: notice.path.clone(),
        };
        strip.paint(buf, w, h, notice_y);
    }

    // Overlay popup (omnibar / command / find).
    if let Some(bar) = overlay_data {
        let ModalPanel {
            x: popup_x,
            y: popup_y,
            w: popup_w,
            h: popup_h,
            inner_x,
            inner_y,
            inner_w,
            inner_h,
        } = ModalPanel::omnibar(win_w, height, bar.total_height());
        fill_rect_u32(
            buf,
            w,
            h,
            popup_x as usize,
            popup_y as usize,
            popup_w as usize,
            popup_h as usize,
            OMNIBAR_POPUP_BORDER_COLOR,
        );
        fill_rect_u32(
            buf,
            w,
            h,
            inner_x as usize,
            inner_y as usize,
            inner_w as usize,
            inner_h as usize,
            OMNIBAR_POPUP_BG,
        );
        bar.paint_at(
            buf,
            w,
            h,
            inner_x as usize,
            inner_y as usize,
            inner_w as usize,
            inner_h as usize,
        );
    }

    // Context-menu overlay. Rendered last so it floats above all other
    // chrome layers. Coordinates are clamped by the widget itself.
    if let Some(menu) = context_menu {
        menu.paint(buf, w, h);
    }
}
