//! [`WebKitEngine`] — phase 1 stub [`BrowserEngine`] impl for WPE WebKit.
//!
//! Every method either no-ops (fire-and-forget) or returns a safe default.
//! No actual WPE WebKit calls are made in this phase — the worker thread parks
//! immediately and all OSR surfaces are empty but valid.
//!
//! Phase 2 will wire real WebView instances via the GLib worker thread.

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

use super::error::WebKitError;
use super::worker::{WorkerHandle, spawn};

// ── WebKitEngine ──────────────────────────────────────────────────────────────

/// WPE WebKit browser engine (phase 1 — all methods are stubs).
///
/// Implements [`BrowserEngine`] (`Send + Sync`) by holding thread-safe handles.
/// The GLib worker thread parks immediately; real WPE init comes in Phase 2.
pub struct WebKitEngine {
    #[allow(dead_code)]
    engine_id: EngineId,
    /// Shared OSR frame — empty/black in Phase 1.
    frame: SharedOsrFrame,
    /// Shared OSR viewport state.
    view: SharedOsrViewState,
    /// Worker thread handle — parked in Phase 1.
    #[allow(dead_code)]
    worker: WorkerHandle,
    /// OSR wake callback. Installed by apps layer; called on each paint.
    wake: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// Popup sinks — empty in Phase 1.
    popup_queue: PopupQueue,
    popup_create_sink: PopupCreateSink,
    popup_close_sink: PopupCloseSink,
}

impl WebKitEngine {
    /// Construct a new (Phase 1 stub) engine.
    pub fn new(options: &BackendOpenOptions<'_>) -> Result<Self, WebKitError> {
        tracing::debug!("webkit: WebKitEngine::new (phase 1 stub)");
        let (width, height) = options.initial_size;

        let frame: SharedOsrFrame = Arc::new(Mutex::new(OsrFrame::new(width, height)));
        let view: SharedOsrViewState = Arc::new(OsrViewState::default());

        let worker = spawn()?;

        Ok(Self {
            engine_id: options.engine_id.clone(),
            frame,
            view,
            worker,
            wake: Mutex::new(None),
            popup_queue: new_popup_queue(),
            popup_create_sink: new_popup_create_sink(),
            popup_close_sink: new_popup_close_sink(),
        })
    }
}

// ── BrowserEngine impl — all stubs ────────────────────────────────────────────

impl BrowserEngine for WebKitEngine {
    // ── Lifecycle ─────────────────────────────────────────────────────────────

    fn close_all_browsers(&self) {
        tracing::debug!("webkit: close_all_browsers stub");
    }

    // ── Tabs ──────────────────────────────────────────────────────────────────

    fn open_tab(&self, _url: &str) -> Result<TabId, EngineError> {
        tracing::debug!("webkit: open_tab stub");
        Ok(TabId(0))
    }

    fn open_tab_background(&self, _url: &str) -> Result<TabId, EngineError> {
        tracing::debug!("webkit: open_tab_background stub");
        Ok(TabId(0))
    }

    fn open_tab_at(&self, _url: &str, _insert_idx: usize) -> Result<TabId, EngineError> {
        tracing::debug!("webkit: open_tab_at stub");
        Ok(TabId(0))
    }

    fn close_tab(&self, _id: TabId) -> Result<bool, EngineError> {
        tracing::debug!("webkit: close_tab stub");
        Ok(true)
    }

    fn close_active(&self) -> Result<bool, EngineError> {
        tracing::debug!("webkit: close_active stub");
        Ok(true)
    }

    fn select_tab(&self, _id: TabId) {
        tracing::debug!("webkit: select_tab stub");
    }

    fn next_tab(&self) {
        tracing::debug!("webkit: next_tab stub");
    }

    fn prev_tab(&self) {
        tracing::debug!("webkit: prev_tab stub");
    }

    fn move_tab(&self, _from: usize, _to: usize) {
        tracing::debug!("webkit: move_tab stub");
    }

    fn duplicate_active(&self) -> Result<TabId, EngineError> {
        tracing::debug!("webkit: duplicate_active stub");
        Ok(TabId(0))
    }

    fn toggle_pin_active(&self) {
        tracing::debug!("webkit: toggle_pin_active stub");
    }

    fn set_pinned(&self, _id: TabId, _pinned: bool) {
        tracing::debug!("webkit: set_pinned stub");
    }

    fn reopen_closed_tab(&self) -> Result<Option<TabId>, EngineError> {
        tracing::debug!("webkit: reopen_closed_tab stub");
        Ok(None)
    }

    fn closed_stack_len(&self) -> usize {
        0
    }

    fn active_tab(&self) -> Option<TabSummary> {
        None
    }

    fn tabs_summary(&self) -> Vec<TabSummary> {
        Vec::new()
    }

    fn tab_count(&self) -> usize {
        0
    }

    fn pinned_count(&self) -> usize {
        0
    }

    fn active_index(&self) -> Option<usize> {
        None
    }

    // ── Navigation ────────────────────────────────────────────────────────────

    fn navigate(&self, _url: &str) -> Result<(), EngineError> {
        tracing::debug!("webkit: navigate stub");
        Ok(())
    }

    fn active_tab_live_url(&self) -> String {
        String::new()
    }

    fn pump_address_changes(&self) -> bool {
        false
    }

    // ── Viewport ──────────────────────────────────────────────────────────────

    fn resize(&self, _width: u32, _height: u32) {
        tracing::debug!("webkit: resize stub");
    }

    fn set_device_scale(&self, _scale: f32) {
        tracing::debug!("webkit: set_device_scale stub");
    }

    fn set_frame_rate(&self, _hz: u32) {
        tracing::debug!("webkit: set_frame_rate stub");
    }

    fn notify_screen_info_changed(&self) {
        tracing::debug!("webkit: notify_screen_info_changed stub");
    }

    fn osr_resize(&self, _width: u32, _height: u32) {
        tracing::debug!("webkit: osr_resize stub");
    }

    // ── Input ─────────────────────────────────────────────────────────────────

    fn osr_key_event(&self, _event: NeutralKeyEvent) {
        tracing::debug!("webkit: osr_key_event stub");
    }

    fn osr_mouse_move(&self, _x: i32, _y: i32, _modifiers: u32) {
        tracing::debug!("webkit: osr_mouse_move stub");
    }

    fn osr_mouse_click(
        &self,
        _x: i32,
        _y: i32,
        _button: MouseButton,
        _mouse_up: bool,
        _click_count: i32,
        _modifiers: u32,
    ) {
        tracing::debug!("webkit: osr_mouse_click stub");
    }

    fn osr_mouse_leave(&self, _modifiers: u32) {
        tracing::debug!("webkit: osr_mouse_leave stub");
    }

    fn osr_mouse_wheel(&self, _x: i32, _y: i32, _delta_x: i32, _delta_y: i32, _modifiers: u32) {
        tracing::debug!("webkit: osr_mouse_wheel stub");
    }

    fn osr_focus(&self, _focused: bool) {
        tracing::debug!("webkit: osr_focus stub");
    }

    // ── OSR state ─────────────────────────────────────────────────────────────

    fn osr_frame(&self) -> SharedOsrFrame {
        Arc::clone(&self.frame)
    }

    fn osr_view(&self) -> SharedOsrViewState {
        Arc::clone(&self.view)
    }

    fn force_repaint_active(&self) {
        tracing::debug!("webkit: force_repaint_active stub");
    }

    fn osr_sleep(&self, _sleep: bool) {
        tracing::debug!("webkit: osr_sleep stub");
    }

    fn osr_invalidate_view(&self) {
        tracing::debug!("webkit: osr_invalidate_view stub");
    }

    fn set_osr_wake(&self, wake: Arc<dyn Fn() + Send + Sync>) {
        tracing::debug!("webkit: set_osr_wake stub");
        if let Ok(mut guard) = self.wake.lock()
            && guard.is_none()
        {
            *guard = Some(wake);
        }
    }

    // ── Find / zoom ───────────────────────────────────────────────────────────

    fn start_find(&self, _query: &str, _forward: bool) {
        tracing::debug!("webkit: start_find stub");
    }

    fn stop_find(&self) {
        tracing::debug!("webkit: stop_find stub");
    }

    fn active_zoom_level(&self) -> f64 {
        1.0
    }

    // ── Audio / video ─────────────────────────────────────────────────────────

    fn any_audio_active(&self) -> bool {
        false
    }

    fn any_video_active(&self) -> bool {
        false
    }

    // ── Popup sinks ───────────────────────────────────────────────────────────

    fn popup_queue(&self) -> PopupQueue {
        self.popup_queue.clone()
    }

    fn popup_create_sink(&self) -> PopupCreateSink {
        self.popup_create_sink.clone()
    }

    fn popup_close_sink(&self) -> PopupCloseSink {
        self.popup_close_sink.clone()
    }

    fn popup_resize(&self, _browser_id: i32, _width: u32, _height: u32) {
        tracing::debug!("webkit: popup_resize stub");
    }

    fn popup_close(&self, _browser_id: i32) {
        tracing::debug!("webkit: popup_close stub");
    }

    fn popup_drain_address_changes(&self) -> Vec<(i32, String)> {
        Vec::new()
    }

    fn popup_drain_title_changes(&self) -> Vec<(i32, String)> {
        Vec::new()
    }

    fn popup_history_back(&self, _browser_id: i32) {
        tracing::debug!("webkit: popup_history_back stub");
    }

    fn popup_history_forward(&self, _browser_id: i32) {
        tracing::debug!("webkit: popup_history_forward stub");
    }

    fn popup_osr_focus(&self, _browser_id: i32, _focused: bool) {
        tracing::debug!("webkit: popup_osr_focus stub");
    }

    fn popup_osr_key_event(&self, _browser_id: i32, _event: NeutralKeyEvent) {
        tracing::debug!("webkit: popup_osr_key_event stub");
    }

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
        tracing::debug!("webkit: popup_osr_mouse_click stub");
    }

    fn popup_osr_mouse_move(&self, _browser_id: i32, _x: i32, _y: i32, _modifiers: u32) {
        tracing::debug!("webkit: popup_osr_mouse_move stub");
    }

    fn popup_osr_mouse_wheel(
        &self,
        _browser_id: i32,
        _x: i32,
        _y: i32,
        _delta_x: i32,
        _delta_y: i32,
        _modifiers: u32,
    ) {
        tracing::debug!("webkit: popup_osr_mouse_wheel stub");
    }

    // ── Hint mode ─────────────────────────────────────────────────────────────

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
        tracing::debug!("webkit: cancel_hint stub");
    }
}
