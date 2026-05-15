//! [`ServoEngine`] — [`BrowserEngine`] implementation backed by libservo.
//!
//! # Phase B status: shape-complete skeleton
//!
//! The full engine driver is implemented with the correct architecture, types,
//! and method bodies.  However, **real Servo types cannot be imported** due to
//! a transitive dep conflict:
//!
//!   servo 0.1 → servo-storage 0.1 → rusqlite 0.37 → libsqlite3-sys ^0.35
//!   workspace → buffr-history → rusqlite 0.39 → libsqlite3-sys ^0.37
//!
//! Cargo cannot unify `libsqlite3-sys 0.35.x` and `0.37.x` simultaneously.
//! See `Cargo.toml` for the full analysis and resolution path.
//!
//! # What is functional
//!
//! - `ServoBackend::new()` / `initialize()` — fully functional
//! - `open_engine()` — starts a worker thread (skeleton mode)
//! - `open_tab` / `close_tab` / `select_tab` / `next_tab` / `prev_tab` —
//!   worker tracks tab list in memory (no real page loading)
//! - `close_all_browsers`, `close_active`, `open_tab_background`,
//!   `open_tab_at` — functional tab management
//! - `active_tab`, `tabs_summary`, `tab_count`, `active_index` — live from worker
//! - `osr_frame`, `osr_view`, `set_osr_wake` — stable Arc handles returned
//! - `osr_resize` — forwarded to worker, OSR dims updated
//! - `osr_key_event`, `osr_mouse_move`, `osr_mouse_click`, `osr_mouse_leave`,
//!   `osr_mouse_wheel` — translated and sent to worker (logged, not forwarded to Servo)
//!
//! # What returns `Unimplemented` / no-op
//!
//! - `navigate` — wired through the worker, which records the URL on the
//!   active tab and returns InitFailed (dep conflict). Caller sees the error
//!   but `active_tab_live_url` reflects the requested URL.
//! - `duplicate_active`, `reopen_closed_tab` — no worker support (genuine
//!   `Unimplemented`)
//! - `Command::GoBack` / `GoForward` / `Reload` / `Stop` worker variants exist
//!   but are unreachable from the trait surface today. Phase C wires them in
//!   via `dispatch(&PageAction::Back/Forward/Reload/StopLoading)`.
//! - All popup, hint, find, zoom, devtools, clipboard methods
//!
//! # Architecture (ready for real Servo when conflict is resolved)
//!
//! ```text
//! ServoEngine (any thread)          buffr-servo-worker thread
//! ─────────────────────────         ──────────────────────────
//! WorkerHandle
//!   cmd_tx ─── Command ──────────▶  run()
//!                                    ├─ TODO: servo.spin_event_loop()
//!                                    ├─ TODO: WebViewDelegate callbacks
//!                                    └─ TODO: ctx.read_to_image() → BGRA → SharedOsrFrame
//! ```

use std::sync::{Arc, Mutex};

use buffr_engine::{
    BackendOpenOptions, BrowserEngine, EngineError, HintAction, HintStatus, MouseButton,
    NeutralKeyEvent, OsrFrame, OsrViewState, SharedOsrFrame, SharedOsrViewState, TabId, TabSummary,
    engine_id::EngineId,
    popup::{
        PopupCloseSink, PopupCreateSink, PopupQueue, new_popup_close_sink, new_popup_create_sink,
        new_popup_queue,
    },
};

use crate::error::ServoError;
use crate::input::{
    neutral_click_to_servo, neutral_key_to_servo, neutral_leave_to_servo, neutral_move_to_servo,
    neutral_scroll_to_servo,
};
use crate::worker::{Command, WorkerHandle, spawn};

// ── Tab snapshot cache ────────────────────────────────────────────────────────

/// Cached snapshot of the worker's tab list.
///
/// Refreshed lazily on each call to `tabs_summary()` / `active_tab()`.
#[derive(Default)]
struct TabCache {
    /// Tabs in strip order.
    summaries: Vec<TabSummary>,
    /// Index of the active tab.
    active_idx: Option<usize>,
}

// ── ServoEngine ───────────────────────────────────────────────────────────────

/// Servo browser engine driver.
///
/// Implements [`BrowserEngine`] by delegating all mutable Servo operations to
/// a worker thread via [`WorkerHandle`].
pub struct ServoEngine {
    #[allow(dead_code)]
    engine_id: EngineId,
    /// Handle to the worker thread.
    worker: WorkerHandle,
    /// Shared OSR frame — the worker writes BGRA pixels here after each paint.
    frame: SharedOsrFrame,
    /// Shared viewport state — read by `osr_view()`.
    view: SharedOsrViewState,
    /// Cached tab snapshot (protected by Mutex for &self access).
    cache: Mutex<TabCache>,
}

impl ServoEngine {
    /// Construct the engine, spawn the worker thread, and open the initial URL.
    pub fn new(options: &BackendOpenOptions<'_>) -> Result<Self, ServoError> {
        let (width, height) = options.initial_size;

        // Create the shared OSR state that both the worker and the engine hold.
        let frame: SharedOsrFrame = Arc::new(Mutex::new(OsrFrame::new(width, height)));
        let view: SharedOsrViewState = {
            let v = OsrViewState::new();
            v.width.store(width, std::sync::atomic::Ordering::Relaxed);
            v.height.store(height, std::sync::atomic::Ordering::Relaxed);
            Arc::new(v)
        };

        let worker = spawn(
            options.initial_url,
            width,
            height,
            Arc::clone(&frame),
            Arc::clone(&view),
        )?;

        tracing::info!(
            "servo engine created (engine_id={}, url={}, size={width}×{height})",
            options.engine_id.as_str(),
            options.initial_url,
        );

        Ok(ServoEngine {
            engine_id: options.engine_id.clone(),
            worker,
            frame,
            view,
            cache: Mutex::new(TabCache::default()),
        })
    }

    /// Fetch a fresh tab snapshot from the worker and update the cache.
    fn refresh_cache(&self) {
        let snapshot = self
            .worker
            .call(|reply| Command::QueryTabs { reply })
            .unwrap_or_else(|_| crate::worker::TabsSnapshot {
                tabs: vec![],
                active: None,
            });

        let summaries: Vec<TabSummary> = snapshot.tabs.iter().map(|t| t.to_summary()).collect();
        let active_idx = snapshot
            .active
            .and_then(|id| summaries.iter().position(|s| s.id == id));

        if let Ok(mut guard) = self.cache.lock() {
            guard.summaries = summaries;
            guard.active_idx = active_idx;
        }
    }
}

impl Drop for ServoEngine {
    fn drop(&mut self) {
        tracing::info!("servo engine: shutting down");
        self.worker.send(Command::Shutdown);
    }
}

// ── BrowserEngine impl ────────────────────────────────────────────────────────

impl BrowserEngine for ServoEngine {
    // ── Lifecycle ────────────────────────────────────────────────────────────

    fn close_all_browsers(&self) {
        tracing::debug!("servo: close_all_browsers");
        // Fetch current tab list and close each one.
        self.refresh_cache();
        let ids: Vec<TabId> = self
            .cache
            .lock()
            .map(|g| g.summaries.iter().map(|s| s.id).collect())
            .unwrap_or_default();
        for id in ids {
            let _ = self.worker.call(|reply| Command::CloseTab { id, reply });
        }
    }

    // ── Tabs ─────────────────────────────────────────────────────────────────

    fn open_tab(&self, url: &str) -> Result<TabId, EngineError> {
        // Skeleton: worker creates a tab record in memory but no real Servo WebView.
        tracing::debug!("servo: open_tab {url} (skeleton — no real page loading)");
        self.worker
            .call(|reply| Command::OpenTab {
                url: url.to_owned(),
                reply,
            })
            .map_err(EngineError::from)?
            .map_err(EngineError::from)
    }

    fn open_tab_background(&self, url: &str) -> Result<TabId, EngineError> {
        // Servo does not distinguish foreground/background — open and select.
        // For "background", we open and then re-select the previously active tab.
        let active_before = self.active_tab().map(|t| t.id);
        let id = self.open_tab(url)?;
        if let Some(prev) = active_before {
            self.select_tab(prev);
        }
        Ok(id)
    }

    fn open_tab_at(&self, url: &str, _insert_idx: usize) -> Result<TabId, EngineError> {
        // Servo doesn't natively support inserting at an arbitrary index in Phase B.
        // Fall through to append-and-activate, same as open_tab.
        tracing::debug!("servo: open_tab_at {url} (insert_idx ignored in Phase B)");
        self.open_tab(url)
    }

    fn close_tab(&self, id: TabId) -> Result<bool, EngineError> {
        tracing::debug!("servo: close_tab {id:?}");
        self.worker
            .call(|reply| Command::CloseTab { id, reply })
            .map_err(EngineError::from)?
            .map_err(EngineError::from)
    }

    fn close_active(&self) -> Result<bool, EngineError> {
        self.refresh_cache();
        let active = self
            .cache
            .lock()
            .ok()
            .and_then(|g| g.active_idx.and_then(|i| g.summaries.get(i).map(|s| s.id)));
        if let Some(id) = active {
            self.close_tab(id)
        } else {
            Ok(false)
        }
    }

    fn select_tab(&self, id: TabId) {
        tracing::debug!("servo: select_tab {id:?}");
        self.worker.send(Command::SelectTab { id });
    }

    fn next_tab(&self) {
        self.worker.send(Command::CycleTab { forward: true });
    }

    fn prev_tab(&self) {
        self.worker.send(Command::CycleTab { forward: false });
    }

    fn move_tab(&self, _from: usize, _to: usize) {
        tracing::debug!("servo: move_tab not implemented in Phase B");
    }

    fn duplicate_active(&self) -> Result<TabId, EngineError> {
        Err(EngineError::Unimplemented {
            method: "duplicate_active",
        })
    }

    fn toggle_pin_active(&self) {
        tracing::debug!("servo: toggle_pin_active not implemented");
    }

    fn set_pinned(&self, _id: TabId, _pinned: bool) {
        tracing::debug!("servo: set_pinned not implemented");
    }

    fn reopen_closed_tab(&self) -> Result<Option<TabId>, EngineError> {
        Err(EngineError::Unimplemented {
            method: "reopen_closed_tab",
        })
    }

    fn closed_stack_len(&self) -> usize {
        0
    }

    fn active_tab(&self) -> Option<TabSummary> {
        self.refresh_cache();
        self.cache
            .lock()
            .ok()
            .and_then(|g| g.active_idx.and_then(|i| g.summaries.get(i).cloned()))
    }

    fn tabs_summary(&self) -> Vec<TabSummary> {
        self.refresh_cache();
        self.cache
            .lock()
            .map(|g| g.summaries.clone())
            .unwrap_or_default()
    }

    fn tab_count(&self) -> usize {
        self.refresh_cache();
        self.cache.lock().map(|g| g.summaries.len()).unwrap_or(0)
    }

    fn pinned_count(&self) -> usize {
        0
    }

    fn active_index(&self) -> Option<usize> {
        self.refresh_cache();
        self.cache.lock().ok().and_then(|g| g.active_idx)
    }

    // ── Navigation ───────────────────────────────────────────────────────────

    fn navigate(&self, url: &str) -> Result<(), EngineError> {
        // Skeleton: the worker records the requested URL on the active tab and
        // returns InitFailed("servo: libsqlite3-sys dep conflict…") because no
        // real WebView exists. The error propagates to the apps layer; the URL
        // still shows up in the cache so address-bar reads behave reasonably.
        tracing::debug!("servo: navigate {url} (skeleton — dep conflict)");
        self.worker
            .call(|reply| Command::Navigate {
                url: url.to_owned(),
                reply,
            })
            .map_err(EngineError::from)?
            .map_err(EngineError::from)
    }

    fn active_tab_live_url(&self) -> String {
        self.active_tab().map(|t| t.url).unwrap_or_default()
    }

    fn pump_address_changes(&self) -> bool {
        // Tab URL is refreshed on every `refresh_cache()` call; always return
        // `true` to prompt the apps layer to re-read the address bar.
        true
    }

    // ── Viewport ─────────────────────────────────────────────────────────────

    fn resize(&self, width: u32, height: u32) {
        // `resize` and `osr_resize` are aliased — both forward to the worker.
        self.worker.send(Command::Resize { width, height });
    }

    fn set_device_scale(&self, scale: f32) {
        tracing::debug!("servo: set_device_scale {scale} (not yet forwarded to worker)");
        self.view.set_scale(scale);
    }

    fn set_frame_rate(&self, _hz: u32) {
        tracing::debug!("servo: set_frame_rate not implemented in Phase B");
    }

    fn notify_screen_info_changed(&self) {
        tracing::debug!("servo: notify_screen_info_changed — no-op in Phase B");
    }

    fn osr_resize(&self, width: u32, height: u32) {
        tracing::debug!("servo: osr_resize {width}×{height}");
        // Mark the frame dirty so callers know to wait for a fresh paint.
        if let Ok(mut guard) = self.frame.lock() {
            guard.needs_fresh = true;
        }
        self.worker.send(Command::Resize { width, height });
    }

    // ── Input ────────────────────────────────────────────────────────────────

    fn osr_key_event(&self, event: NeutralKeyEvent) {
        let servo_ev = neutral_key_to_servo(&event);
        self.worker.send(Command::SendInput { event: servo_ev });
    }

    fn osr_mouse_move(&self, x: i32, y: i32, _modifiers: u32) {
        let ev = neutral_move_to_servo(x, y);
        self.worker.send(Command::SendInput { event: ev });
    }

    fn osr_mouse_click(
        &self,
        x: i32,
        y: i32,
        button: MouseButton,
        mouse_up: bool,
        _click_count: i32,
        _modifiers: u32,
    ) {
        let ev = neutral_click_to_servo(x, y, button, mouse_up);
        self.worker.send(Command::SendInput { event: ev });
    }

    fn osr_mouse_leave(&self, _modifiers: u32) {
        let ev = neutral_leave_to_servo();
        self.worker.send(Command::SendInput { event: ev });
    }

    fn osr_mouse_wheel(&self, x: i32, y: i32, delta_x: i32, delta_y: i32, _modifiers: u32) {
        let ev = neutral_scroll_to_servo(x, y, delta_x, delta_y);
        self.worker.send(Command::SendInput { event: ev });
    }

    fn osr_focus(&self, _focused: bool) {
        tracing::debug!("servo: osr_focus not implemented in Phase B");
    }

    // ── OSR state ────────────────────────────────────────────────────────────

    fn osr_frame(&self) -> SharedOsrFrame {
        Arc::clone(&self.frame)
    }

    fn osr_view(&self) -> SharedOsrViewState {
        Arc::clone(&self.view)
    }

    fn force_repaint_active(&self) {
        tracing::debug!("servo: force_repaint_active — no-op in Phase B");
    }

    fn osr_sleep(&self, sleep: bool) {
        tracing::debug!("servo: osr_sleep({sleep}) — no-op in Phase B");
    }

    fn osr_invalidate_view(&self) {
        tracing::debug!("servo: osr_invalidate_view — no-op in Phase B");
    }

    fn set_osr_wake(&self, wake: Arc<dyn Fn() + Send + Sync>) {
        self.view.set_wake(wake);
        tracing::debug!("servo: wake callback installed");
    }

    // ── Find / zoom ──────────────────────────────────────────────────────────

    fn start_find(&self, _query: &str, _forward: bool) {
        tracing::debug!("servo: start_find not implemented in Phase B");
    }

    fn stop_find(&self) {
        tracing::debug!("servo: stop_find not implemented in Phase B");
    }

    fn active_zoom_level(&self) -> f64 {
        self.worker
            .call(|reply| Command::QueryActiveZoom { reply })
            .unwrap_or(1.0)
    }

    fn zoom_in(&self) {
        let next = (self.active_zoom_level() + 0.1).min(5.0);
        self.worker.send(Command::SetZoom(next));
    }

    fn zoom_out(&self) {
        let next = (self.active_zoom_level() - 0.1).max(0.25);
        self.worker.send(Command::SetZoom(next));
    }

    fn zoom_reset(&self) {
        self.worker.send(Command::SetZoom(1.0));
    }

    // ── Audio / video ────────────────────────────────────────────────────────

    fn any_audio_active(&self) -> bool {
        false
    }

    fn any_video_active(&self) -> bool {
        false
    }

    // ── Navigation history capability ─────────────────────────────────────────

    fn can_go_back(&self) -> bool {
        // Phase B: worker tracks can_go_back per WebView but the TabSummary
        // struct has no dedicated field for it yet.  Always false is the safe
        // conservative answer (matching blink-cdp's default).
        false
    }

    fn can_go_forward(&self) -> bool {
        false
    }

    // ── Popup stubs ──────────────────────────────────────────────────────────

    fn popup_queue(&self) -> PopupQueue {
        new_popup_queue()
    }

    fn popup_create_sink(&self) -> PopupCreateSink {
        new_popup_create_sink()
    }

    fn popup_close_sink(&self) -> PopupCloseSink {
        new_popup_close_sink()
    }

    fn popup_resize(&self, _browser_id: i32, _width: u32, _height: u32) {
        tracing::debug!("servo: popup_resize not implemented");
    }

    fn popup_close(&self, _browser_id: i32) {
        tracing::debug!("servo: popup_close not implemented");
    }

    fn popup_drain_address_changes(&self) -> Vec<(i32, String)> {
        vec![]
    }

    fn popup_drain_title_changes(&self) -> Vec<(i32, String)> {
        vec![]
    }

    fn popup_history_back(&self, _browser_id: i32) {}

    fn popup_history_forward(&self, _browser_id: i32) {}

    fn popup_osr_focus(&self, _browser_id: i32, _focused: bool) {}

    fn popup_osr_key_event(&self, _browser_id: i32, _event: NeutralKeyEvent) {}

    fn popup_osr_mouse_click(
        &self,
        _browser_id: i32,
        _x: i32,
        _y: i32,
        _button: MouseButton,
        _mouse_up: bool,
        _click_count: i32,
        _modifiers: u32,
    ) {
    }

    fn popup_osr_mouse_move(&self, _browser_id: i32, _x: i32, _y: i32, _modifiers: u32) {}

    fn popup_osr_mouse_wheel(
        &self,
        _browser_id: i32,
        _x: i32,
        _y: i32,
        _delta_x: i32,
        _delta_y: i32,
        _modifiers: u32,
    ) {
    }

    // ── Hint mode stubs ───────────────────────────────────────────────────────

    fn is_hint_mode(&self) -> bool {
        false
    }

    fn hint_status(&self) -> Option<HintStatus> {
        None
    }

    fn pump_hint_events(&self) -> bool {
        false
    }

    fn feed_hint_key(&self, _c: char) -> Option<HintAction> {
        None
    }

    fn backspace_hint(&self) -> Option<HintAction> {
        None
    }

    fn cancel_hint(&self) {
        tracing::debug!("servo: cancel_hint not implemented");
    }
}
