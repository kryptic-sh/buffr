//! [`WebKitGtkEngine`] — stub [`BrowserEngine`] impl for the WebKitGTK backend.

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

use super::error::WebKitGtkError;

/// WebKitGTK browser engine stub (Linux only).
///
/// Phase A: all methods return `EngineError::Unimplemented`. Phase B will
/// wire real WebKitGTK integration.
pub struct WebKitGtkEngine {
    engine_id: EngineId,
    // Phase B will add real fields.
}

impl WebKitGtkEngine {
    pub fn new(options: &BackendOpenOptions<'_>) -> Result<Self, WebKitGtkError> {
        Ok(Self {
            engine_id: options.engine_id.clone(),
        })
    }
}

impl BrowserEngine for WebKitGtkEngine {
    fn close_all_browsers(&self) {
        tracing::debug!("webkitgtk::close_all_browsers not yet implemented");
    }

    fn open_tab(&self, _url: &str) -> Result<TabId, EngineError> {
        Err(EngineError::Unimplemented { method: "open_tab" })
    }

    fn open_tab_background(&self, _url: &str) -> Result<TabId, EngineError> {
        Err(EngineError::Unimplemented {
            method: "open_tab_background",
        })
    }

    fn open_tab_at(&self, _url: &str, _insert_idx: usize) -> Result<TabId, EngineError> {
        Err(EngineError::Unimplemented {
            method: "open_tab_at",
        })
    }

    fn close_tab(&self, _id: TabId) -> Result<bool, EngineError> {
        Err(EngineError::Unimplemented {
            method: "close_tab",
        })
    }

    fn close_active(&self) -> Result<bool, EngineError> {
        Err(EngineError::Unimplemented {
            method: "close_active",
        })
    }

    fn select_tab(&self, _id: TabId) {
        tracing::debug!("webkitgtk::select_tab not yet implemented");
    }

    fn next_tab(&self) {
        tracing::debug!("webkitgtk::next_tab not yet implemented");
    }

    fn prev_tab(&self) {
        tracing::debug!("webkitgtk::prev_tab not yet implemented");
    }

    fn move_tab(&self, _from: usize, _to: usize) {
        tracing::debug!("webkitgtk::move_tab not yet implemented");
    }

    fn duplicate_active(&self) -> Result<TabId, EngineError> {
        Err(EngineError::Unimplemented {
            method: "duplicate_active",
        })
    }

    fn toggle_pin_active(&self) {
        tracing::debug!("webkitgtk::toggle_pin_active not yet implemented");
    }

    fn set_pinned(&self, _id: TabId, _pinned: bool) {
        tracing::debug!("webkitgtk::set_pinned not yet implemented");
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
        None
    }

    fn tabs_summary(&self) -> Vec<TabSummary> {
        vec![]
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

    fn navigate(&self, _url: &str) -> Result<(), EngineError> {
        Err(EngineError::Unimplemented { method: "navigate" })
    }

    fn active_tab_live_url(&self) -> String {
        String::new()
    }

    fn pump_address_changes(&self) -> bool {
        false
    }

    fn resize(&self, _width: u32, _height: u32) {
        tracing::debug!("webkitgtk::resize not yet implemented");
    }

    fn set_device_scale(&self, _scale: f32) {
        tracing::debug!("webkitgtk::set_device_scale not yet implemented");
    }

    fn set_frame_rate(&self, _hz: u32) {
        tracing::debug!("webkitgtk::set_frame_rate not yet implemented");
    }

    fn notify_screen_info_changed(&self) {
        tracing::debug!("webkitgtk::notify_screen_info_changed not yet implemented");
    }

    fn osr_resize(&self, _width: u32, _height: u32) {
        tracing::debug!("webkitgtk::osr_resize not yet implemented");
    }

    fn osr_key_event(&self, _event: NeutralKeyEvent) {
        tracing::debug!("webkitgtk::osr_key_event not yet implemented");
    }

    fn osr_mouse_move(&self, _x: i32, _y: i32, _modifiers: u32) {
        tracing::debug!("webkitgtk::osr_mouse_move not yet implemented");
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
        tracing::debug!("webkitgtk::osr_mouse_click not yet implemented");
    }

    fn osr_mouse_leave(&self, _modifiers: u32) {
        tracing::debug!("webkitgtk::osr_mouse_leave not yet implemented");
    }

    fn osr_mouse_wheel(&self, _x: i32, _y: i32, _delta_x: i32, _delta_y: i32, _modifiers: u32) {
        tracing::debug!("webkitgtk::osr_mouse_wheel not yet implemented");
    }

    fn osr_focus(&self, _focused: bool) {
        tracing::debug!("webkitgtk::osr_focus not yet implemented");
    }

    fn osr_frame(&self) -> SharedOsrFrame {
        Arc::new(Mutex::new(OsrFrame::new(1, 1)))
    }

    fn osr_view(&self) -> SharedOsrViewState {
        Arc::new(OsrViewState::default())
    }

    fn force_repaint_active(&self) {
        tracing::debug!("webkitgtk::force_repaint_active not yet implemented");
    }

    fn osr_sleep(&self, _sleep: bool) {
        tracing::debug!("webkitgtk::osr_sleep not yet implemented");
    }

    fn osr_invalidate_view(&self) {
        tracing::debug!("webkitgtk::osr_invalidate_view not yet implemented");
    }

    fn set_osr_wake(&self, _wake: Arc<dyn Fn() + Send + Sync>) {
        tracing::debug!("webkitgtk::set_osr_wake not yet implemented");
    }

    fn start_find(&self, _query: &str, _forward: bool) {
        tracing::debug!("webkitgtk::start_find not yet implemented");
    }

    fn stop_find(&self) {
        tracing::debug!("webkitgtk::stop_find not yet implemented");
    }

    fn active_zoom_level(&self) -> f64 {
        1.0
    }

    fn any_audio_active(&self) -> bool {
        false
    }

    fn any_video_active(&self) -> bool {
        false
    }

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
        tracing::debug!("webkitgtk::popup_resize not yet implemented");
    }

    fn popup_close(&self, _browser_id: i32) {
        tracing::debug!("webkitgtk::popup_close not yet implemented");
    }

    fn popup_drain_address_changes(&self) -> Vec<(i32, String)> {
        vec![]
    }

    fn popup_drain_title_changes(&self) -> Vec<(i32, String)> {
        vec![]
    }

    fn popup_history_back(&self, _browser_id: i32) {
        tracing::debug!("webkitgtk::popup_history_back not yet implemented");
    }

    fn popup_history_forward(&self, _browser_id: i32) {
        tracing::debug!("webkitgtk::popup_history_forward not yet implemented");
    }

    fn popup_osr_focus(&self, _browser_id: i32, _focused: bool) {
        tracing::debug!("webkitgtk::popup_osr_focus not yet implemented");
    }

    fn popup_osr_key_event(&self, _browser_id: i32, _event: NeutralKeyEvent) {
        tracing::debug!("webkitgtk::popup_osr_key_event not yet implemented");
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
        tracing::debug!("webkitgtk::popup_osr_mouse_click not yet implemented");
    }

    fn popup_osr_mouse_move(&self, _browser_id: i32, _x: i32, _y: i32, _modifiers: u32) {
        tracing::debug!("webkitgtk::popup_osr_mouse_move not yet implemented");
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
        tracing::debug!("webkitgtk::popup_osr_mouse_wheel not yet implemented");
    }

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
        tracing::debug!("webkitgtk::cancel_hint not yet implemented");
    }
}
