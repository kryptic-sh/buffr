//! Off-screen rendering (OSR) shared frame buffer + RenderHandler.
//!
//! ## Architecture
//!
//! CEF's OSR path skips all windowed embedding:
//!
//! ```text
//!   +--------------+    on_paint(BGRA, w, h)    +----------------------+
//!   |  CEF (OSR)   | -------------------------> |  OsrPaintHandler     |
//!   |  no window   |                            |  → SharedOsrFrame    |
//!   +--------------+                            +----------+-----------+
//!                                                          |
//!                                                          v
//!                                              +-----------+----------+
//!                                              |   step 4 compositor  |
//!                                              |   (winit surface)    |
//!                                              +----------------------+
//! ```
//!
//! [`OsrPaintHandler`] implements CEF's `RenderHandler` trait. It writes
//! raw BGRA pixels into a [`SharedOsrFrame`] on every `on_paint` call and
//! bumps a monotonic `generation` counter so downstream compositors can
//! skip work when nothing changed.
//!
//! [`OsrViewState`] holds the current viewport dimensions as atomics so
//! both the CEF IO thread (reading from `view_rect`) and the UI thread
//! (writing via `BrowserHost::osr_resize`) can access them without a mutex.
//!
//! ## Multi-browser routing
//!
//! `OsrPaintHandler` is created once per CEF `Client` (one per tab) but
//! popup browsers created by `on_before_popup` share the same client path;
//! their browser id differs from the main tab's id.  The handler stores the
//! main browser id at construction time and a shared map of popup
//! `(frame, view)` pairs. On every CEF callback it routes by
//! `browser.identifier()`:
//!
//! - matches `main_id` → use main frame / view
//! - found in `popup_frames` → use that pair
//! - unknown → skip with a trace log

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};

use cef::*;

// Re-export the engine-agnostic OSR types so all cef-internal code and
// the apps layer can import them from one place without a direct
// `buffr_engine` dep.
pub use buffr_engine::{OsrFrame, OsrViewState, SharedOsrFrame, SharedOsrViewState};

/// Map of popup browser OSR state, keyed by CEF `browser.identifier()`.
/// Shared between `BrowserHost` (which inserts/removes entries) and
/// `OsrPaintHandler` (which reads them on CEF IO callbacks).
pub type PopupFrameMap = Arc<Mutex<HashMap<i32, (SharedOsrFrame, SharedOsrViewState)>>>;

/// One popup's OSR allocation, made in `on_before_popup` before CEF has
/// assigned a browser id and claimed in `on_after_created` once it has.
pub struct PopupAlloc {
    pub frame: SharedOsrFrame,
    pub view: SharedOsrViewState,
    pub url: String,
    /// When the alloc was pushed (A12): an entry older than
    /// [`POPUP_ALLOC_TTL`] was left behind by a popup creation CEF
    /// aborted, and the next real popup must not consume its stale URL.
    pub created_at: std::time::Instant,
}

/// FIFO of pending popup allocations (M16).
///
/// This replaces `buffr_engine::PendingPopupAlloc`, which was a single
/// `Option` slot: two `window.open()` calls in one task fire
/// `on_before_popup` twice before either `on_after_created`, so the second
/// alloc overwrote the first. The first popup then rendered the second's
/// URL and the second was dropped entirely — never inserted into
/// `popup_frames`/`popup_browsers`, so `close_all_browsers` missed it and a
/// live CEF browser leaked into `cef::shutdown()`.
///
/// A FIFO is the correct shape rather than a `HashMap<popup_id, _>`:
/// `cef_life_span_handler_t::on_after_created` takes only `(self, browser)`
/// — the `popup_id` that `on_before_popup` receives is **not** available on
/// the creation side, so there is nothing to key on. CEF creates popups in
/// the order they were requested and both callbacks are sequenced on the
/// browser-process UI thread, so front-of-queue is the matching alloc.
pub type PendingPopupAllocQueue = Arc<Mutex<VecDeque<PopupAlloc>>>;

/// Upper bound on queued-but-unclaimed popup allocations.
///
/// Each entry holds an 800×600 BGRA frame buffer (~1.9 MiB), so an
/// unbounded queue is a memory-growth vector for a page that calls
/// `window.open()` in a loop while CEF blocks the popups. Oldest entries
/// are evicted first.
pub const PENDING_POPUP_ALLOC_CAP: usize = 32;

/// Build an empty [`PendingPopupAllocQueue`].
pub fn new_pending_popup_alloc_queue() -> PendingPopupAllocQueue {
    Arc::new(Mutex::new(VecDeque::new()))
}

/// How long a pending popup allocation may sit unclaimed before it is
/// treated as stale (A12). CEF sequences `on_before_popup` →
/// `on_after_created` on the same UI thread within milliseconds, so an
/// alloc older than this was left behind by a popup creation CEF
/// aborted.
pub const POPUP_ALLOC_TTL: std::time::Duration = std::time::Duration::from_secs(2);

/// Pop the front allocation that is still fresh, discarding any stale
/// entries ahead of it (A12). Pure in `now`/`ttl` so the expiry policy
/// is unit-testable.
pub fn take_fresh_alloc(
    queue: &mut std::collections::VecDeque<PopupAlloc>,
    now: std::time::Instant,
    ttl: std::time::Duration,
) -> Option<PopupAlloc> {
    loop {
        let a = queue.pop_front()?;
        if now.duration_since(a.created_at) <= ttl {
            return Some(a);
        }
        tracing::warn!(
            url = %a.url,
            "popup: dropping stale pending alloc (creation was aborted)"
        );
    }
}

// ── RenderHandler impl ─────────────────────────────────────────────────────────

// `loading_busy`: cleared on every successful main-frame `on_paint`.
// Set by `BuffrLoadHandler::on_load_start` so the embedder can show
// a loading animation across the navigation gap and stop it the
// moment the next paint commits.
wrap_render_handler! {
    pub struct OsrPaintHandler {
        main_id: Arc<AtomicI32>,
        frame: SharedOsrFrame,
        view: SharedOsrViewState,
        popup_frames: PopupFrameMap,
        loading_busy: Arc<AtomicBool>,
    }

    impl RenderHandler {
        fn view_rect(&self, browser: Option<&mut Browser>, rect: Option<&mut Rect>) {
            let Some(rect) = rect else { return };
            let (w, h) = self.resolve_dims(browser.as_deref().map(|b| b.identifier()));
            rect.x = 0;
            rect.y = 0;
            rect.width = w as i32;
            rect.height = h as i32;
            // No debug log here — view_rect is called on every frame by the CEF
            // compositor thread and the log volume would swamp all other output.
        }

        fn screen_info(
            &self,
            browser: Option<&mut Browser>,
            screen_info: Option<&mut ScreenInfo>,
        ) -> ::std::os::raw::c_int {
            let Some(si) = screen_info else {
                tracing::trace!("osr: screen_info — screen_info arg is None");
                return 0;
            };
            let browser_id = browser.as_deref().map(|b| b.identifier());
            let (w, h) = self.resolve_dims(browser_id);
            let scale = self.resolve_scale(browser_id);
            // No debug log here — screen_info is called on every frame paint
            // alongside view_rect; log volume would swamp all other output.
            si.device_scale_factor = scale;
            si.depth = 32;
            si.depth_per_component = 8;
            si.is_monochrome = 0;
            si.rect = Rect {
                x: 0,
                y: 0,
                width: w as i32,
                height: h as i32,
            };
            si.available_rect = si.rect.clone();
            1
        }

        fn screen_point(
            &self,
            _browser: Option<&mut Browser>,
            view_x: ::std::os::raw::c_int,
            view_y: ::std::os::raw::c_int,
            screen_x: Option<&mut ::std::os::raw::c_int>,
            screen_y: Option<&mut ::std::os::raw::c_int>,
        ) -> ::std::os::raw::c_int {
            // No multi-monitor positioning yet — view coords == screen coords.
            if let Some(sx) = screen_x {
                *sx = view_x;
            }
            if let Some(sy) = screen_y {
                *sy = view_y;
            }
            1
        }

        // The `buffer` raw pointer is provided by CEF and is valid for
        // `width * height * 4` bytes for the duration of this call. The
        // lint fires because the trait method signature contains `*const u8`,
        // but the safety obligation is on CEF, not on our call site.
        #[allow(clippy::not_unsafe_ptr_arg_deref)]
        fn on_paint(
            &self,
            browser: Option<&mut Browser>,
            type_: PaintElementType,
            _dirty_rects: Option<&[Rect]>,
            buffer: *const u8,
            width: ::std::os::raw::c_int,
            height: ::std::os::raw::c_int,
        ) {
            // Only handle the main View paint. Popup compositing is deferred.
            if type_.get_raw() != PaintElementType::VIEW.get_raw() {
                tracing::trace!("osr: on_paint Popup — deferred (TODO: composite popup)");
                return;
            }

            let browser_id = browser.as_deref().map(|b| b.identifier());

            // Validate the FFI inputs BEFORE touching the pointer (M17).
            // `width`/`height` are `c_int`: a negative value would
            // sign-extend through `as u32` into a ~4-billion-element `len`,
            // and `slice::from_raw_parts(null, 0)` is UB even at zero
            // length. CEF should never hand us either, but this is the
            // trust boundary.
            let Some((w, h, len)) = paint_buffer_len(buffer, width, height) else {
                tracing::warn!(
                    width,
                    height,
                    buffer_null = buffer.is_null(),
                    ?browser_id,
                    "osr: on_paint — invalid buffer/dimensions, skipping"
                );
                return;
            };
            tracing::trace!(w, h, ?browser_id, "osr: on_paint fired");

            // Route to the correct (frame, view) pair FIRST — no reason to
            // materialise a slice over CEF's buffer for a paint we are
            // going to drop.
            let (frame, view) = match self.resolve_frame_view(browser_id) {
                Some(pair) => pair,
                None => {
                    tracing::trace!(
                        ?browser_id,
                        "osr: on_paint — unknown browser id, skipping"
                    );
                    return;
                }
            };

            // SAFETY: `buffer` is non-null and `len == width * height * 4`
            // with both dimensions validated positive above; CEF guarantees
            // those bytes stay valid for the duration of this call.
            let src = unsafe { std::slice::from_raw_parts(buffer, len) };

            let Ok(mut guard) = frame.lock() else {
                tracing::warn!("osr: on_paint — frame mutex poisoned, skipping");
                return;
            };

            // Resize the backing buffer when dimensions change OR when the
            // buffer length doesn't match expected — the embedder may have
            // taken/swapped the Vec out (mem::swap with a scratch buffer)
            // and left this side with an empty Vec while dims are unchanged.
            if guard.width != w || guard.height != h || guard.pixels.len() != len {
                if guard.width != w || guard.height != h {
                    tracing::debug!(
                        old_w = guard.width,
                        old_h = guard.height,
                        new_w = w,
                        new_h = h,
                        "osr: on_paint dimension change",
                    );
                }
                guard.pixels.resize(len, 0);
                guard.width = w;
                guard.height = h;
            }

            guard.pixels.copy_from_slice(src);
            guard.generation = guard.generation.wrapping_add(1);
            // Pair with `BrowserHost::osr_resize` setting this true.
            // The next gate-check on the embedder side now treats this
            // paint as a real post-resize commit, regardless of whether
            // its dims happen to match a recent `osr_view`.
            guard.needs_fresh = false;
            drop(guard);
            // First contentful paint after a navigation clears the
            // loading-busy gate — embedder stops the loading anim.
            self.loading_busy.store(false, Ordering::Relaxed);
            // Wake the embedder so the UI loop can pump a redraw.
            if let Some(wake) = view.wake.get() {
                wake();
            }
        }
    }
}

/// Validate CEF's `on_paint` buffer + dimensions and compute the BGRA
/// byte length.
///
/// Returns `None` — meaning "drop this paint" — when the buffer pointer is
/// null, either dimension is non-positive, or `width * height * 4` would
/// overflow `usize`. See M17: `width`/`height` arrive as `c_int` and a
/// negative value sign-extends through `as u32` into a ~4-billion-element
/// length, and `slice::from_raw_parts(null, 0)` is UB even at zero length.
fn paint_buffer_len(buffer: *const u8, width: i32, height: i32) -> Option<(u32, u32, usize)> {
    if buffer.is_null() || width <= 0 || height <= 0 {
        return None;
    }
    let w = width as u32;
    let h = height as u32;
    let len = (w as usize).checked_mul(h as usize)?.checked_mul(4)?;
    Some((w, h, len))
}

impl OsrPaintHandler {
    /// True when `id` is a registered popup browser (A3). A popup must never
    /// claim the main slot: a background tab whose popup paints before the tab
    /// itself would otherwise have `main_id` stolen, dropping the main tab's
    /// paints forever.
    fn is_registered_popup(&self, id: i32) -> bool {
        self.popup_frames
            .lock()
            .map(|m| m.contains_key(&id))
            .unwrap_or(false)
    }

    /// Resolve the scale factor for the given browser id.
    fn resolve_scale(&self, browser_id: Option<i32>) -> f32 {
        if let Some(id) = browser_id {
            let main = self.main_id.load(Ordering::Relaxed);
            if main == id || main == -1 {
                return self.view.scale();
            }
            if let Ok(map) = self.popup_frames.lock()
                && let Some((_, popup_view)) = map.get(&id)
            {
                return popup_view.scale();
            }
        }
        self.view.scale()
    }

    /// Resolve (width, height) for the given browser id.
    fn resolve_dims(&self, browser_id: Option<i32>) -> (u32, u32) {
        if let Some(id) = browser_id {
            // Check if this is the known main id. A popup must never claim the
            // main slot (A3): a background tab whose popup paints before the
            // tab itself would otherwise get `main_id` stolen by the popup,
            // dropping the main tab's paints forever. Registration in
            // `popup_frames` (on_after_created) precedes any popup paint, so
            // membership is the reliable discriminator.
            let main = self.main_id.load(Ordering::Relaxed);
            if !self.is_registered_popup(id) && (main == id || main == -1) {
                // Set main_id lazily on first callback.
                if main == -1 {
                    self.main_id.store(id, Ordering::Relaxed);
                }
                return (
                    self.view.width.load(Ordering::Relaxed),
                    self.view.height.load(Ordering::Relaxed),
                );
            }
            // Check popup map.
            if let Ok(map) = self.popup_frames.lock()
                && let Some((_, popup_view)) = map.get(&id)
            {
                return (
                    popup_view.width.load(Ordering::Relaxed),
                    popup_view.height.load(Ordering::Relaxed),
                );
            }
        }
        // Fallback: use main view dims.
        (
            self.view.width.load(Ordering::Relaxed),
            self.view.height.load(Ordering::Relaxed),
        )
    }

    /// Resolve the (frame, view) pair for the given browser id.
    /// Returns `None` if the id is unknown (not main, not a popup).
    fn resolve_frame_view(
        &self,
        browser_id: Option<i32>,
    ) -> Option<(SharedOsrFrame, SharedOsrViewState)> {
        let id = browser_id?;
        let main = self.main_id.load(Ordering::Relaxed);
        // A popup must never claim the main slot (A3): a background tab whose
        // popup paints before the tab itself would otherwise get main_id stolen
        // by the popup, dropping the main tab's paints forever. Registration in
        // `popup_frames` (on_after_created) precedes any popup paint, so
        // membership is the reliable discriminator.
        if !self.is_registered_popup(id) && (main == -1 || main == id) {
            if main == -1 {
                self.main_id.store(id, Ordering::Relaxed);
            }
            return Some((self.frame.clone(), self.view.clone()));
        }
        // Check popup map.
        if let Ok(map) = self.popup_frames.lock()
            && let Some((pf, pv)) = map.get(&id)
        {
            return Some((pf.clone(), pv.clone()));
        }
        None
    }
}

/// Construct a new [`OsrPaintHandler`] for a single main-tab browser.
///
/// `popup_frames` is shared with [`BrowserHost`] so popup entries can be
/// inserted/removed without rebuilding the handler.
pub fn make_osr_paint_handler(
    frame: SharedOsrFrame,
    view: SharedOsrViewState,
    popup_frames: PopupFrameMap,
    loading_busy: Arc<AtomicBool>,
) -> RenderHandler {
    OsrPaintHandler::new(
        Arc::new(AtomicI32::new(-1)),
        frame,
        view,
        popup_frames,
        loading_busy,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // OsrViewState is read by CEF's IO thread inside `view_rect` /
    // `screen_info`. The embedder writes via `BrowserHost::osr_resize`
    // (dims) and `BrowserHost::set_device_scale` (scale). These tests
    // pin the contract: scale and dims are independent atomics; nothing
    // in `set_scale` may touch the dim atomics, and nothing in dim
    // writes may touch scale. Past bugs confused `BrowserHost::resize`
    // (which leaves osr_view untouched) with `osr_resize` (which writes
    // them) — the regression bites when chrome layout changes without
    // a window resize.

    // ── M17: on_paint FFI-input validation ────────────────────────────────

    #[test]
    fn paint_buffer_len_rejects_null_buffer() {
        assert!(paint_buffer_len(std::ptr::null(), 1280, 800).is_none());
        // Null is rejected even at a zero-area rect — `from_raw_parts(null, 0)`
        // is UB regardless of length.
        assert!(paint_buffer_len(std::ptr::null(), 0, 0).is_none());
    }

    #[test]
    fn paint_buffer_len_rejects_non_positive_dims() {
        let buf = [0u8; 16];
        let p = buf.as_ptr();
        assert!(paint_buffer_len(p, 0, 800).is_none());
        assert!(paint_buffer_len(p, 1280, 0).is_none());
        // The sign-extension bug: `-1 as u32` == 4_294_967_295.
        assert!(paint_buffer_len(p, -1, 800).is_none());
        assert!(paint_buffer_len(p, 1280, -1).is_none());
        assert!(paint_buffer_len(p, i32::MIN, i32::MIN).is_none());
    }

    #[test]
    fn paint_buffer_len_accepts_valid_dims() {
        let buf = [0u8; 16];
        let p = buf.as_ptr();
        assert_eq!(paint_buffer_len(p, 2, 2), Some((2, 2, 16)));
        assert_eq!(paint_buffer_len(p, 1280, 800), Some((1280, 800, 4_096_000)));
    }

    #[test]
    fn paint_buffer_len_rejects_overflow() {
        let buf = [0u8; 16];
        let p = buf.as_ptr();
        // Only reachable on 32-bit targets; on 64-bit the product fits, so
        // just assert the function is total (never panics) for i32::MAX.
        let got = paint_buffer_len(p, i32::MAX, i32::MAX);
        if usize::BITS <= 32 {
            assert!(got.is_none());
        } else {
            assert!(got.is_some());
        }
    }

    #[test]
    fn default_view_dims_and_scale() {
        let v = OsrViewState::new();
        assert_eq!(v.width.load(Ordering::Relaxed), 1280);
        assert_eq!(v.height.load(Ordering::Relaxed), 800);
        assert!((v.scale() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn set_scale_does_not_touch_dims() {
        let v = OsrViewState::new();
        v.width.store(1500, Ordering::Relaxed);
        v.height.store(1050, Ordering::Relaxed);
        v.set_scale(2.0);
        assert_eq!(v.width.load(Ordering::Relaxed), 1500);
        assert_eq!(v.height.load(Ordering::Relaxed), 1050);
        assert!((v.scale() - 2.0).abs() < 1e-6);
    }

    #[test]
    fn set_scale_round_trips_thousandths() {
        let v = OsrViewState::new();
        v.set_scale(1.25);
        assert!((v.scale() - 1.25).abs() < 1e-3);
        v.set_scale(1.5);
        assert!((v.scale() - 1.5).abs() < 1e-3);
        v.set_scale(2.0);
        assert!((v.scale() - 2.0).abs() < 1e-3);
    }

    #[test]
    fn set_scale_clamps_to_at_least_one_thousandth() {
        // Encoded as Q1000; floor = 1 thousandth = 0.001×. Guards against
        // CEF receiving scale=0 if the embedder ever passes a degenerate
        // value (BUFFR_SCALE override, monitor-yank race).
        let v = OsrViewState::new();
        v.set_scale(0.0);
        assert!(v.scale() > 0.0);
    }

    #[test]
    fn dim_writes_independent_of_scale() {
        // Mirror what BrowserHost::osr_resize does — two atomic stores.
        // Verify scale survives.
        let v = OsrViewState::new();
        v.set_scale(1.5);
        v.width.store(2000, Ordering::Relaxed);
        v.height.store(1400, Ordering::Relaxed);
        assert!((v.scale() - 1.5).abs() < 1e-3);
        assert_eq!(v.width.load(Ordering::Relaxed), 2000);
        assert_eq!(v.height.load(Ordering::Relaxed), 1400);
    }

    #[test]
    fn popup_cannot_claim_the_main_slot() {
        let popup_frames: PopupFrameMap = Arc::new(Mutex::new(HashMap::new()));
        let main_frame: SharedOsrFrame = Arc::new(Mutex::new(OsrFrame::new(1, 1)));
        let main_view = Arc::new(OsrViewState::new());
        let popup_frame: SharedOsrFrame = Arc::new(Mutex::new(OsrFrame::new(2, 2)));
        let popup_view = Arc::new(OsrViewState::new());
        // `resolve_dims` reads the *view* atomics, so give the popup view
        // distinct dims for the routing assertion below.
        popup_view.width.store(2, Ordering::Relaxed);
        popup_view.height.store(2, Ordering::Relaxed);
        // Popup registered and paints FIRST (background tab): must route to
        // the popup pair and NOT claim the main slot. The struct is built
        // directly (not via `OsrPaintHandler::new`, which returns the
        // `cef::RenderHandler` wrapper and hides the private members).
        let handler = OsrPaintHandler {
            main_id: Arc::new(AtomicI32::new(-1)),
            frame: main_frame.clone(),
            view: main_view.clone(),
            popup_frames: popup_frames.clone(),
            loading_busy: Arc::new(AtomicBool::new(false)),
            cef_object: std::ptr::null_mut(),
        };
        popup_frames
            .lock()
            .unwrap()
            .insert(42, (popup_frame.clone(), popup_view.clone()));
        let (pf, pv) = handler
            .resolve_frame_view(Some(42))
            .expect("popup routes via popup_frames");
        assert!(Arc::ptr_eq(&pf, &popup_frame));
        assert!(Arc::ptr_eq(&pv, &popup_view));
        assert_eq!(
            handler.main_id.load(Ordering::Relaxed),
            -1,
            "popup must not claim main_id"
        );
        // resolve_dims for the popup returns the popup's dims and does not claim.
        let (w, h) = handler.resolve_dims(Some(42));
        assert_eq!((w, h), (2, 2));
        assert_eq!(
            handler.main_id.load(Ordering::Relaxed),
            -1,
            "popup must not claim main_id via dims"
        );
        // The main tab's first paint then claims the slot and routes to the main pair.
        let (mf, mv) = handler
            .resolve_frame_view(Some(7))
            .expect("main routes to main pair");
        assert!(Arc::ptr_eq(&mf, &main_frame));
        assert!(Arc::ptr_eq(&mv, &main_view));
        assert_eq!(handler.main_id.load(Ordering::Relaxed), 7);
    }

    // ── A12: pending-alloc TTL expiry ────────────────────────────────────

    /// An alloc with a fresh `created_at`, matching what `on_before_popup`
    /// pushes. The old code (3-tuple, no timestamp) had no expiry at all —
    /// `on_after_created` popped the front unconditionally, so an alloc left
    /// behind by an aborted popup creation reported its stale URL on the next
    /// real popup. These tests pin the expiry policy.
    fn alloc(created_at: std::time::Instant) -> PopupAlloc {
        PopupAlloc {
            frame: Arc::new(Mutex::new(OsrFrame::new(800, 600))),
            view: Arc::new(OsrViewState::new()),
            url: String::from("about:blank"),
            created_at,
        }
    }

    #[test]
    fn take_fresh_alloc_skips_stale_front() {
        let now = std::time::Instant::now();
        let mut q = VecDeque::new();
        q.push_back(alloc(now - std::time::Duration::from_secs(10)));
        q.push_back(alloc(now - std::time::Duration::from_millis(1)));
        let got = take_fresh_alloc(&mut q, now, POPUP_ALLOC_TTL).expect("fresh alloc returned");
        assert_eq!(got.created_at, now - std::time::Duration::from_millis(1));
        assert!(q.is_empty(), "stale front discarded, fresh consumed");
    }

    #[test]
    fn take_fresh_alloc_all_stale_returns_none() {
        let now = std::time::Instant::now();
        let mut q = VecDeque::new();
        q.push_back(alloc(now - std::time::Duration::from_secs(10)));
        q.push_back(alloc(now - std::time::Duration::from_secs(20)));
        assert!(take_fresh_alloc(&mut q, now, POPUP_ALLOC_TTL).is_none());
        assert!(q.is_empty(), "all stale allocs drained");
    }

    #[test]
    fn take_fresh_alloc_empty_returns_none() {
        let mut q: VecDeque<PopupAlloc> = VecDeque::new();
        assert!(take_fresh_alloc(&mut q, std::time::Instant::now(), POPUP_ALLOC_TTL).is_none());
    }

    #[test]
    fn take_fresh_alloc_fresh_front() {
        let now = std::time::Instant::now();
        let mut q = VecDeque::new();
        q.push_back(alloc(now - std::time::Duration::from_millis(1)));
        let got = take_fresh_alloc(&mut q, now, POPUP_ALLOC_TTL).expect("fresh alloc returned");
        assert_eq!(got.created_at, now - std::time::Duration::from_millis(1));
        assert!(q.is_empty());
    }
}
