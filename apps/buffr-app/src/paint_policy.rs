//! Pure decision helpers for the paint path.
//!
//! Everything here answers "what should this frame do?" without
//! touching `AppState`, CEF or the GPU: which paint path to take,
//! whether the window looks occluded, when a resize has settled,
//! where the tab strip and CEF child rect land, and whether an OSR
//! frame is fresh enough to commit.
//!
//! They are split out precisely because they are pure -- the unit
//! tests drive them directly with constructed inputs, which is not
//! possible for the event-loop code that calls them.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use buffr_ui::{DOWNLOAD_NOTICE_HEIGHT, STATUSLINE_HEIGHT, TAB_STRIP_HEIGHT};

use crate::chrome_paint::popup_bar_h_physical;
use crate::{PRESENT_HISTORY_SIZE, PaintPolicy};

// ---------------------------------------------------------------------------
// Group 1: Paint dispatch
// ---------------------------------------------------------------------------

/// Which paint code path `paint_chrome_with` should take for this frame.
///
/// Priority (highest first):
///
/// 1. `Animation`  — `want_anim = true`; show loading animation.
/// 2. `FreshOsr`   — fresh on_paint pixels in `osr_meta`.
/// 3. `SyntheticScratch` — between paints; use cached `osr_scratch`.
/// 4. `DeadFallback` — no paint ever received.
///
/// The ordering is intentional: a size-mismatch triggers `want_anim`
/// even when a fresh `osr_meta` arrived this frame (v0.1.25 invariant
/// that prevents a momentary flash of a wrong-sized OSR quad).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaintPath {
    Animation,
    FreshOsr,
    SyntheticScratch,
    DeadFallback,
}

// ---------------------------------------------------------------------------
// Group 0 (internal): Physical-pixel → DIP conversion helpers
// ---------------------------------------------------------------------------

/// Convert a physical-pixel cursor position (window-relative) to the
/// CEF OSR coordinate space (DIP, browser-region-relative).
///
/// `phys_x` / `phys_y` — cursor in physical pixels relative to the
///     window origin.
/// `cef_y_offset` — top of the CEF region in physical pixels (chrome
///     strips above it).
/// `scale` — current device scale factor.
///
/// Returns DIP coords relative to the CEF browser region's top-left.
///
/// Degenerate `scale <= 0.0` is clamped to 1.0 to avoid division by zero.
/// Cursor above the CEF region (phys_y < cef_y_offset) produces a negative
/// DIP y — callers that care about clamping must do so themselves.
/// Convert physical window dims to the logical (DIP) chrome-buffer dims.
///
/// The chrome CPU buffer is allocated at logical size and GPU-stretched,
/// so every chrome-space geometry computation — painting AND hit-testing
/// — must agree on this conversion. Both results are clamped to ≥1.
pub(crate) fn logical_chrome_dims(phys_w: u32, phys_h: u32, scale: f32) -> (u32, u32) {
    let scale = if scale <= 0.0 { 1.0 } else { scale };
    let lw = ((phys_w as f32) / scale).round() as u32;
    let lh = ((phys_h as f32) / scale).round() as u32;
    (lw.max(1), lh.max(1))
}

pub(crate) fn physical_cursor_to_dip(
    phys_x: i32,
    phys_y: i32,
    cef_y_offset: u32,
    scale: f32,
) -> (i32, i32) {
    let scale = if scale <= 0.0 { 1.0 } else { scale };
    let region_y = (phys_y).saturating_sub(cef_y_offset as i32);
    let bx = ((phys_x as f32) / scale).round() as i32;
    let by = ((region_y as f32) / scale).round() as i32;
    (bx, by)
}

// ---------------------------------------------------------------------------
// OSR sleep policy helpers
// ---------------------------------------------------------------------------

/// Pure policy decision: should the window's OSR paint pipeline be active?
///
/// `Active` when the window is visible (not occluded) OR media is playing.
/// `Sleeping` only when occluded AND no media — invisible to the user and
/// nothing audible to maintain.
///
/// Driven by `WindowEvent::Occluded`, not `Focused`: a side-by-side window
/// that is visible but unfocused must keep painting.
///
/// All callers must route through this function; no inline duplication of
/// the predicate is allowed.  Tests pin the semantics so future refactors
/// can't silently change the rule.
pub(crate) fn decide_paint_policy(occluded: bool, _media_active: bool) -> PaintPolicy {
    // Media flag is intentionally not consulted: empirical testing
    // confirmed CEF was_hidden(1) keeps the audio thread alive on Linux,
    // so a YouTube tab on a hidden workspace continues playing audio
    // while we skip wgpu present.  Detection of media activity stays
    // wired up for #22 idle-inhibit (keep screen awake) but no longer
    // gates the sleep policy.
    if !occluded {
        PaintPolicy::Active
    } else {
        PaintPolicy::Sleeping
    }
}

/// Push a present-time sample into the rolling history, evicting the
/// oldest if at capacity.  Pure to keep the heuristic testable.
pub(crate) fn record_present_us(history: &mut VecDeque<u64>, present_us: u64) {
    history.push_back(present_us);
    while history.len() > PRESENT_HISTORY_SIZE {
        history.pop_front();
    }
}

/// Decide whether the rolling `present_us` history indicates the
/// surface is currently occluded by the compositor.  At least
/// `min_slow` samples must exceed `slow_threshold_us`.  Pure for
/// testability — callers pass thresholds explicitly so unit tests
/// don't depend on the production constants.
pub(crate) fn detect_occluded_from_history(
    history: &VecDeque<u64>,
    slow_threshold_us: u64,
    min_slow: usize,
) -> bool {
    history.iter().filter(|&&us| us > slow_threshold_us).count() >= min_slow
}

/// Pure paint-path decision. All callers must route through this function;
/// no inline duplication of the predicate is allowed. Tests pin the
/// priority so future refactors can't silently re-order the arms.
pub(crate) fn decide_paint_path(
    want_anim: bool,
    has_osr_meta: bool,
    last_osr_dims: Option<(u32, u32)>,
) -> PaintPath {
    if want_anim {
        PaintPath::Animation
    } else if has_osr_meta {
        PaintPath::FreshOsr
    } else if last_osr_dims.is_some() {
        PaintPath::SyntheticScratch
    } else {
        PaintPath::DeadFallback
    }
}

// ---------------------------------------------------------------------------
// Group 2: Resize debounce state machine
// ---------------------------------------------------------------------------

/// Owns the "quiet after last Resized" deadline for CEF resize calls.
///
/// `arm` refreshes the deadline on every `WindowEvent::Resized`.
/// `should_fire` returns true once the deadline elapses.
/// `clear` consumes the pending entry and returns the last queued dims.
/// `deadline` is read by `about_to_wait` to set `ControlFlow::WaitUntil`.
///
/// The *dims* stored here are advisory — the flush in `about_to_wait`
/// recomputes them from live window + chrome state to avoid the stale-
/// dim bug (debounce flush using queued dims that pre-dated a notice expiry).
#[derive(Debug, Default)]
pub(crate) struct ResizeDebounce {
    pending: Option<(u32, u32, Instant)>,
}

impl ResizeDebounce {
    /// Arm or re-arm the debounce.  Each call during a resize drag
    /// pushes the deadline `debounce` into the future, so the flush
    /// only fires once the drag is quiet.
    pub(crate) fn arm(&mut self, w: u32, h: u32, now: Instant, debounce: Duration) {
        self.pending = Some((w, h, now + debounce));
    }

    /// True when a pending resize is overdue.
    pub(crate) fn should_fire(&self, now: Instant) -> bool {
        self.pending.is_some_and(|(_, _, at)| now >= at)
    }

    /// Consume the pending entry.  Returns the queued dims if one was
    /// present, `None` if the debounce was already unarmed.
    pub(crate) fn clear(&mut self) -> Option<(u32, u32)> {
        self.pending.take().map(|(w, h, _)| (w, h))
    }

    /// The deadline instant, if a resize is pending.  `about_to_wait`
    /// clamps `ControlFlow::WaitUntil` to this so the loop wakes exactly
    /// when the debounce expires.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.pending.map(|(_, _, at)| at)
    }
}

// ---------------------------------------------------------------------------
// Group 2a: Resize-paint watchdog state machine
// ---------------------------------------------------------------------------

/// Watchdog that detects when CEF fails to emit an on_paint at the
/// expected (post-resize) dims within a deadline, so the embedder
/// can nudge it via a was_hidden cycle (mimicking the tab-switch
/// trick).
///
/// Lifecycle: arm() when osr_resize is called with new dims.
/// observe_paint() each time the freshness gate accepts a paint.
/// In the event loop, check should_force_repaint(now); if true,
/// call BrowserHost::force_repaint_active and record_force_repaint
/// to bump the deadline + retry counter.
///
/// Caps retries at MAX_RETRIES so a genuinely stuck CEF doesn't
/// loop forever.
#[derive(Debug, Default)]
pub(crate) struct ResizePaintWatchdog {
    /// (expected_w, expected_h, deadline, retries_so_far)
    pending: Option<(u32, u32, Instant, u32)>,
}

impl ResizePaintWatchdog {
    /// Maximum number of force-repaint nudges before giving up.
    /// Three is enough to recover from CEF's worst dedup quirks
    /// without burning into an infinite loop on a genuinely stuck
    /// renderer.
    pub(crate) const MAX_RETRIES: u32 = 3;

    pub(crate) fn arm(&mut self, w: u32, h: u32, now: Instant, timeout: Duration) {
        self.pending = Some((w, h, now + timeout, 0));
    }

    /// Called when the freshness gate accepts a paint. Clears the
    /// watchdog if (painted_w, painted_h) match the awaited dims.
    /// Returns true if cleared.
    pub(crate) fn observe_paint(&mut self, painted_w: u32, painted_h: u32) -> bool {
        if let Some((w, h, _, _)) = self.pending
            && painted_w == w
            && painted_h == h
        {
            self.pending = None;
            true
        } else {
            false
        }
    }

    pub(crate) fn should_force_repaint(&self, now: Instant) -> bool {
        self.pending
            .is_some_and(|(_, _, deadline, retries)| now >= deadline && retries < Self::MAX_RETRIES)
    }

    /// Bump deadline + retry counter after firing a force-repaint
    /// nudge. If retry count would exceed MAX_RETRIES, clears the
    /// watchdog (give up).
    pub(crate) fn record_force_repaint(&mut self, now: Instant, timeout: Duration) {
        if let Some((w, h, _, retries)) = self.pending {
            let next_retries = retries + 1;
            if next_retries >= Self::MAX_RETRIES {
                self.pending = None;
            } else {
                self.pending = Some((w, h, now + timeout, next_retries));
            }
        }
    }

    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.pending.map(|(_, _, d, _)| d)
    }

    pub(crate) fn retry_count(&self) -> u32 {
        self.pending.map(|(_, _, _, r)| r).unwrap_or(0)
    }

    #[cfg(test)]
    pub(crate) fn is_armed(&self) -> bool {
        self.pending.is_some()
    }
}

// ---------------------------------------------------------------------------
// Group 3: Tab-strip hit-test (pure)
// ---------------------------------------------------------------------------

/// Pure tab-strip hit-test.  All inputs are in **logical** (DIP) pixels.
///
/// `log_full_w` / `log_full_h` — full window logical size.
/// `log_cursor_x` / `log_cursor_y` — cursor position in window-logical coords
///     (i.e. already converted from physical and offset by `cef_y`).
/// `has_notice` — whether the download-notice strip is visible.
/// `pinned_count` / `total_count` — tab counts; pinned tabs sort first.
///
/// Returns the tab index under the cursor, or `None`.
pub(crate) fn hit_test_tab_strip_pure(
    log_full_w: u32,
    log_full_h: u32,
    log_cursor_x: u32,
    log_cursor_y: u32,
    has_notice: bool,
    pinned_count: u32,
    total_count: u32,
) -> Option<usize> {
    if total_count == 0 {
        return None;
    }

    // Compute tab_y in logical space (matches tab_strip_y).
    let notice_h = if has_notice {
        DOWNLOAD_NOTICE_HEIGHT
    } else {
        0
    };
    let tab_y = notice_h.min(log_full_h);
    let tab_y_end = tab_y + TAB_STRIP_HEIGHT;

    if log_cursor_y < tab_y || log_cursor_y >= tab_y_end {
        return None;
    }

    pub(crate) const GUTTER: u32 = 4;
    let unpinned_count = total_count.saturating_sub(pinned_count);
    let pinned_total_w = pinned_count * buffr_ui::tab_strip::PINNED_TAB_WIDTH;
    let gutter_total = (total_count + 1) * GUTTER;
    let avail_for_unpinned = log_full_w
        .saturating_sub(pinned_total_w)
        .saturating_sub(gutter_total);
    let raw_w = avail_for_unpinned.checked_div(unpinned_count).unwrap_or(0);
    let unpinned_w = raw_w.clamp(buffr_ui::MIN_TAB_WIDTH, buffr_ui::MAX_TAB_WIDTH);

    if log_cursor_x < GUTTER {
        return None;
    }

    // Walk pills left-to-right: pinned tabs first, then unpinned.
    let mut x = GUTTER;
    for i in 0..total_count as usize {
        let pill_w = if (i as u32) < pinned_count {
            buffr_ui::tab_strip::PINNED_TAB_WIDTH
        } else {
            unpinned_w
        };
        let right = x + pill_w;
        if log_cursor_x >= x && log_cursor_x < right {
            return Some(i);
        }
        x = right + GUTTER;
    }
    None
}

/// Pure CEF page-rect computation. Mirrors the body of
/// `AppState::cef_child_rect` / `cef_child_rect_logical` without depending
/// on `&self`, so unit tests can drive every (size × scale × notice) case
/// without constructing an `AppState`.
///
/// `scale = 1.0` produces logical (DIP) coords; pass the device scale for
/// physical-px output.
pub(crate) fn cef_child_rect_pure(
    full_w: u32,
    full_h: u32,
    scale: f32,
    has_notice: bool,
) -> (u32, u32, u32, u32) {
    let scale_up = |h: u32| ((h as f32) * scale).round() as u32;
    let phys_status_h = scale_up(STATUSLINE_HEIGHT);
    let phys_tab_h = scale_up(TAB_STRIP_HEIGHT);
    let phys_notice_h_max = scale_up(DOWNLOAD_NOTICE_HEIGHT);

    let status_h = phys_status_h.min(full_h);
    let remaining_after_status = full_h.saturating_sub(status_h);
    let tab_h = phys_tab_h.min(remaining_after_status);
    let remaining_after_tabs = remaining_after_status.saturating_sub(tab_h);
    let notice_h = if has_notice {
        phys_notice_h_max.min(remaining_after_tabs)
    } else {
        0
    };
    let remaining_after_notice = remaining_after_tabs.saturating_sub(notice_h);
    let cef_w = full_w.max(1);
    let cef_h = remaining_after_notice.max(1);
    let cef_y = notice_h + tab_h;
    (0, cef_y, cef_w, cef_h)
}

/// Pure CEF page-rect computation for a **popup** window, in physical
/// pixels.
///
/// A popup's chrome is one address-bar strip at the top and nothing else,
/// so the page area is the window minus that strip. This is the popup's
/// counterpart to [`cef_child_rect_pure`], and — like it — is the single
/// source of truth for two things that must agree: the `dst_rect` the OSR
/// quad is drawn into, and the viewport handed to `popup_resize` (which
/// becomes CEF's `view_rect`).
///
/// They disagreed until M35: the resize call passed the FULL window height
/// while the quad was painted one bar shorter, so CEF laid the page out for
/// more rows than were displayed and the image was squashed — a vertical
/// error that grows toward the bottom of the window, sending clicks near
/// the bottom edge to the wrong element.
///
/// `full_w` / `full_h` are physical pixels; the returned rect is physical
/// too. The bar height comes from [`popup_bar_h_physical`], the same helper
/// the pointer offset uses, so there is only one rounding of it.
///
/// A window shorter than the bar yields a height of 1 rather than
/// underflowing, matching the paint site's `saturating_sub(..).max(1)`.
pub(crate) fn popup_cef_rect_pure(full_w: u32, full_h: u32, scale: f32) -> (u32, u32, u32, u32) {
    let bar_h = popup_bar_h_physical(scale);
    let cef_w = full_w.max(1);
    let cef_h = full_h.saturating_sub(bar_h).max(1);
    (0, bar_h, cef_w, cef_h)
}

/// Whether the SharedOsrFrame currently holds a freshly-painted CEF
/// frame that the embedder has not yet consumed.
///
/// Four conditions, all required:
///
/// 1. Non-zero dims.
/// 2. `frame_w == expected_w && frame_h == expected_h` — paint is at
///    the dims we asked CEF for via `osr_resize`. Rejects "stale" paints
///    that were in flight on CEF's IO thread when we resized; those
///    paints carry the OLD dims and would, if accepted, pin
///    `last_osr_dims` to old dims while `browser_w/h` (computed from
///    `window.inner_size()`) is new — leaving the loading animation
///    stuck even though a paint did arrive.
/// 3. `pixels.len() == frame_w * frame_h * 4` — guards against the gap
///    between our `mem::swap`-out and the next on_paint, when
///    `frame.pixels` holds a leftover Vec from a previous swap that
///    has the OLD length but the dim atomics now reflect NEW dims.
/// 4. `frame.generation != last_seen_generation` — guards against
///    swapping the same frame in twice (which would put the leftover
///    Vec back into `osr_scratch`, eventually drifting the scratch
///    length out of sync with `last_osr_dims`).
///
/// `expected_w` and `expected_h` are the current `osr_view.{width,height}`
/// atomics (i.e. the dims we last passed to `osr_resize`). The freshness
/// gate is the only place these atomics are consulted on the embedder
/// side — once a paint has been accepted, `last_osr_dims` (which we
/// own) takes over for downstream gating.
///
/// Past bugs:
///   - `!pixels.is_empty()` missed (3) and (4), allowing a bad swap
///     that triggered a wgpu validation panic on the FreshOsr upload.
///   - Missing (2) caused a brief OSR flash at old dims during resize
///     followed by a stuck animation: a stale-dim paint passed the
///     gate, set `last_osr_dims = old dims`, then the loading-anim
///     gate flipped back to true because old dims != new browser_w/h,
///     and CEF's coalesced second invalidate sometimes failed to
///     produce a follow-up on_paint.
#[allow(clippy::too_many_arguments)]
pub(crate) fn is_osr_frame_fresh(
    frame_w: u32,
    frame_h: u32,
    pixels_len: usize,
    frame_generation: u64,
    last_seen_generation: u64,
    expected_w: u32,
    expected_h: u32,
    needs_fresh: bool,
) -> bool {
    let expected_len = (frame_w as usize) * (frame_h as usize) * 4;
    !needs_fresh
        && frame_w > 0
        && frame_h > 0
        && frame_w == expected_w
        && frame_h == expected_h
        && pixels_len == expected_len
        && frame_generation != last_seen_generation
}

/// Post-frame bookkeeping decision for one `Renderer::frame` call.
///
/// See [`decide_frame_commit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct FrameCommit {
    /// Retire the chrome dirty state (`last_painted_chrome_gen =
    /// chrome_generation`).
    pub(crate) advance_chrome_gen: bool,
    /// Feed this `submit_done_us` sample into the occlusion heuristic.
    pub(crate) observe_us: Option<u64>,
    /// Ask for another paint: the pixels never reached the GPU.
    pub(crate) retry_paint: bool,
}

/// Decide what may be committed after a `Renderer::frame` call.
///
/// `outcome` is `None` when `frame()` returned `Err`, otherwise
/// `Some((submit_done_us, submitted))`.
///
/// Rules (H8 / M34):
///
/// - Only a frame that was actually submitted to the render worker may
///   retire the chrome dirty state; a skipped frame uploaded nothing, so
///   erasing the dirty flag would drop the update on the floor.
/// - Only a submitted frame yields a fresh timing sample; a skip returns
///   the previous frame's stats and re-observing them double-counts a
///   single real measurement.
/// - A skip means the paint still owes the user pixels, so it schedules a
///   retry. An `Err` does NOT: errors are sticky in practice and retrying
///   them just burns wakeups.
pub(crate) fn decide_frame_commit(
    outcome: Option<(u64, crate::render::Submitted)>,
    chrome_dirty_effective: bool,
) -> FrameCommit {
    match outcome {
        Some((submit_done_us, crate::render::Submitted::Yes)) => FrameCommit {
            advance_chrome_gen: chrome_dirty_effective,
            observe_us: Some(submit_done_us),
            retry_paint: false,
        },
        Some((_, crate::render::Submitted::No)) => FrameCommit {
            advance_chrome_gen: false,
            observe_us: None,
            retry_paint: true,
        },
        None => FrameCommit::default(),
    }
}

/// Whether the chrome buffer must be re-uploaded to the GPU this frame.
///
/// Three triggers:
/// 1. Chrome state changed (URL, tab list, statusline, etc).
/// 2. Animation is currently active — every tick advances the frame.
/// 3. Animation just deactivated — the chrome texture still holds
///    opaque animation pixels in the browser region from the last
///    paint, and the chrome quad composites OVER the OSR quad. If we
///    don't re-upload a fresh chrome buffer (with the browser region
///    transparent), the animation's last frame occludes the live OSR
///    content. Manifests as "animation stops moving but page never
///    appears" until something else triggers a chrome repaint
///    (scroll, tab switch, key press).
pub(crate) fn should_force_chrome_repaint(
    chrome_dirty: bool,
    want_anim: bool,
    anim_just_deactivated: bool,
) -> bool {
    chrome_dirty || want_anim || anim_just_deactivated
}

/// Decide whether the loading animation should overlay the page region.
///
/// The animation is shown when CEF has not painted at the current
/// `browser_w/h` — either because no `on_paint` has arrived yet
/// (`last_osr_dims = None`) or because the last paint's dims don't match
/// the current rect (CEF still catching up to a resize / chrome-layout
/// change).
///
/// This is the single gate; `paint_chrome_with` calls it once per paint.
/// Unit tests pin the invariant so future refactors can't reintroduce
/// the swap-out flicker (where reading empty `frame.pixels` was used as
/// the gate).
/// Pixel slack tolerated between the host's requested browser rect and the
/// engine's actual delivery dims. WPE WebKit's AcceleratedBackingStore aligns
/// the content area down to a tile boundary, so a requested 1272×623 routinely
/// arrives as 1264×615 — an 8 px shortfall in each axis. Without tolerance the
/// loading animation never deactivates because the gate compares exact pixels.
pub(crate) const OSR_DIM_TOLERANCE: u32 = 32;

pub(crate) fn should_show_loading_anim(
    last_osr_dims: Option<(u32, u32)>,
    browser_w: u32,
    browser_h: u32,
) -> bool {
    let Some((lw, lh)) = last_osr_dims else {
        return true;
    };
    lw.abs_diff(browser_w) > OSR_DIM_TOLERANCE || lh.abs_diff(browser_h) > OSR_DIM_TOLERANCE
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The OSR `dst_rect` the popup paint site draws its quad into,
    /// spelled out independently of [`popup_cef_rect_pure`] so the two
    /// can be compared. Mirrors `AppState::paint_popup_window`.
    fn painted_quad_rect(full_w: u32, full_h: u32, scale: f32) -> (u32, u32, u32, u32) {
        let phys_bar_h = popup_bar_h_physical(scale);
        (
            0,
            phys_bar_h,
            full_w,
            full_h.saturating_sub(phys_bar_h).max(1),
        )
    }

    /// Regression (M35): the viewport handed to `popup_resize` must be the
    /// rect the quad is actually painted into. Before the fix the resize
    /// path passed the full window height while the quad lost an address
    /// bar, so CEF laid the page out taller than it was displayed.
    #[test]
    fn popup_viewport_matches_painted_quad_at_1x() {
        let (w, h, scale) = (800u32, 600u32, 1.0f32);
        assert_eq!(
            popup_cef_rect_pure(w, h, scale),
            painted_quad_rect(w, h, scale)
        );
    }

    #[test]
    fn popup_viewport_matches_painted_quad_at_2x() {
        let (w, h) = (1600u32, 1200u32);
        assert_eq!(popup_cef_rect_pure(w, h, 2.0), painted_quad_rect(w, h, 2.0));

        // The strip is a logical constant, so its physical height doubles
        // with the scale — and the viewport loses exactly that much.
        let bar_1x = popup_cef_rect_pure(w, h, 1.0).1;
        let bar_2x = popup_cef_rect_pure(w, h, 2.0).1;
        assert_eq!(bar_2x, bar_1x * 2, "bar height must scale with the DPI");
        assert_eq!(popup_cef_rect_pure(w, h, 2.0).3, h - bar_2x);
    }

    #[test]
    fn popup_viewport_does_not_underflow_on_tiny_window() {
        let bar = popup_bar_h_physical(1.0);
        for h in [0, 1, bar / 2, bar, bar + 1] {
            let (_, _, cef_w, cef_h) = popup_cef_rect_pure(0, h, 1.0);
            assert!(cef_h >= 1, "height underflowed at full_h={h}: {cef_h}");
            assert!(cef_w >= 1, "width underflowed at full_h={h}: {cef_w}");
        }
    }

    #[test]
    fn popup_viewport_origin_y_is_the_bar_height() {
        for scale in [1.0f32, 1.5, 2.0] {
            let (x, y, _, _) = popup_cef_rect_pure(1024, 768, scale);
            assert_eq!(x, 0);
            assert_eq!(y, popup_bar_h_physical(scale));
            assert_eq!(y, painted_quad_rect(1024, 768, scale).1);
        }
    }
}
