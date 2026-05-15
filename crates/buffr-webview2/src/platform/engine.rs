//! [`WebView2Engine`] — real [`BrowserEngine`] impl for the WebView2 backend.
//!
//! # Phase B status
//!
//! ## Architecture
//!
//! WebView2 objects are COM apartment-threaded (STA). `WebView2Engine` is
//! `Send + Sync` (required by `BrowserEngine`). The engine holds only
//! thread-safe handles and communicates with the STA thread via `mpsc`:
//!
//! ```text
//! WebView2Engine (any thread — Send + Sync)
//!   cmd_tx ──── Command ────▶  worker::StaRuntime (STA thread)
//!                                ├─ CoInitializeEx(COINIT_APARTMENTTHREADED)
//!                                ├─ Win32 message pump
//!                                └─ wires WebView2 events → EngineState
//!   engine_state ────────────────────────────────▶ Arc<Mutex<EngineState>>
//!                (written by event delegates on STA thread; read from any)
//! ```
//!
//! ## Fully wired (real logic, Phase-B in-memory mock)
//!
//! - `close_all_browsers` / `open_tab` / `open_tab_background` / `open_tab_at`
//! - `close_tab` / `close_active`
//! - `select_tab` / `next_tab` / `prev_tab`
//! - `navigate` / `go_back` / `go_forward` / `reload` / `stop`
//! - `can_go_back` / `can_go_forward`
//! - `active_tab` / `tabs_summary` / `tab_count` / `active_index`
//! - `active_tab_live_url`
//! - `osr_resize` / `osr_frame` / `osr_view` / `set_osr_wake`
//! - `force_repaint_active` / `osr_invalidate_view`
//! - `osr_key_event` / `osr_mouse_move` / `osr_mouse_click` /
//!   `osr_mouse_leave` / `osr_mouse_wheel`
//!
//! ## Still stubbed
//!
//! popup, hint, find, zoom, downloads, devtools, scheme, edit, clipboard.
//! Input synthesis is logged but not yet dispatched to the composition
//! controller — see `input.rs` TODO markers.

use std::sync::atomic::Ordering;
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

use super::error::WebView2Error;
use super::input::{
    neutral_click_to_wv2, neutral_key_to_wv2, neutral_leave_to_wv2, neutral_move_to_wv2,
    neutral_scroll_to_wv2,
};
use super::worker::{Command, EngineState, WorkerHandle, spawn};

// ── WebView2Engine ────────────────────────────────────────────────────────────

/// WebView2 browser engine (Windows only).
///
/// Implements [`BrowserEngine`] (`Send + Sync`) by forwarding all WebView2
/// operations to the STA worker thread via a command channel.
pub struct WebView2Engine {
    #[allow(dead_code)]
    engine_id: EngineId,
    /// Shared OSR frame — written by the blank-frame tick.
    frame: SharedOsrFrame,
    /// Shared viewport state.
    view: SharedOsrViewState,
    /// Worker channel handle.
    worker: WorkerHandle,
    /// Thread-safe tab snapshot. Updated by WebView2 event delegates.
    engine_state: Arc<Mutex<EngineState>>,
}

impl WebView2Engine {
    pub fn new(options: &BackendOpenOptions<'_>) -> Result<Self, WebView2Error> {
        let (width, height) = options.initial_size;

        let frame: SharedOsrFrame = Arc::new(Mutex::new(OsrFrame::new(width, height)));
        let view: SharedOsrViewState = {
            let v = OsrViewState::new();
            v.width.store(width, Ordering::Relaxed);
            v.height.store(height, Ordering::Relaxed);
            Arc::new(v)
        };

        let engine_state = Arc::new(Mutex::new(EngineState::new(width, height)));

        let worker = spawn(
            options.initial_url,
            width,
            height,
            Arc::clone(&frame),
            Arc::clone(&view),
            Arc::clone(&engine_state),
        )?;

        tracing::info!(
            "webview2 engine created (id={}, url={}, size={width}×{height})",
            options.engine_id.as_str(),
            options.initial_url,
        );

        Ok(WebView2Engine {
            engine_id: options.engine_id.clone(),
            frame,
            view,
            worker,
            engine_state,
        })
    }

    /// Read from engine_state. Non-blocking; safe from any thread.
    fn with_state<T>(&self, f: impl FnMut(&EngineState) -> T) -> T {
        let mut f = f;
        match self.engine_state.lock() {
            Ok(g) => f(&g),
            Err(_) => {
                let empty = EngineState::new(0, 0);
                f(&empty)
            }
        }
    }
}

impl Drop for WebView2Engine {
    fn drop(&mut self) {
        tracing::info!("webview2 engine: shutting down worker");
        self.worker.send(Command::Shutdown);
    }
}

// ── BrowserEngine impl ────────────────────────────────────────────────────────

impl BrowserEngine for WebView2Engine {
    // ── Lifecycle ────────────────────────────────────────────────────────────

    fn close_all_browsers(&self) {
        tracing::debug!("webview2: close_all_browsers");
        let ids: Vec<TabId> = self.with_state(|st| st.tabs.iter().map(|t| t.id).collect());
        for id in ids {
            let _ = self.worker.call(|reply| Command::CloseTab { id, reply });
        }
    }

    // ── Tabs ─────────────────────────────────────────────────────────────────

    fn open_tab(&self, url: &str) -> Result<TabId, EngineError> {
        tracing::debug!("webview2: open_tab {url}");
        self.worker
            .call(|reply| Command::OpenTab {
                url: url.to_owned(),
                reply,
            })
            .map_err(EngineError::from)?
            .map_err(EngineError::from)
    }

    fn open_tab_background(&self, url: &str) -> Result<TabId, EngineError> {
        let active_before = self.active_tab().map(|t| t.id);
        let id = self.open_tab(url)?;
        if let Some(prev) = active_before {
            self.select_tab(prev);
        }
        Ok(id)
    }

    fn open_tab_at(&self, url: &str, _insert_idx: usize) -> Result<TabId, EngineError> {
        tracing::debug!("webview2: open_tab_at {url} (insert_idx ignored in Phase B)");
        self.open_tab(url)
    }

    fn close_tab(&self, id: TabId) -> Result<bool, EngineError> {
        tracing::debug!("webview2: close_tab {id:?}");
        self.worker
            .call(|reply| Command::CloseTab { id, reply })
            .map_err(EngineError::from)?
            .map_err(EngineError::from)
    }

    fn close_active(&self) -> Result<bool, EngineError> {
        if let Some(id) = self.active_tab().map(|t| t.id) {
            self.close_tab(id)
        } else {
            Ok(false)
        }
    }

    fn select_tab(&self, id: TabId) {
        tracing::debug!("webview2: select_tab {id:?}");
        self.worker.send(Command::SelectTab { id });
    }

    fn next_tab(&self) {
        self.worker.send(Command::CycleTab { forward: true });
    }

    fn prev_tab(&self) {
        self.worker.send(Command::CycleTab { forward: false });
    }

    fn move_tab(&self, _from: usize, _to: usize) {
        tracing::debug!("webview2: move_tab not implemented in Phase B");
    }

    fn duplicate_active(&self) -> Result<TabId, EngineError> {
        Err(EngineError::Unimplemented {
            method: "duplicate_active",
        })
    }

    fn toggle_pin_active(&self) {}
    fn set_pinned(&self, _id: TabId, _pinned: bool) {}

    fn reopen_closed_tab(&self) -> Result<Option<TabId>, EngineError> {
        Err(EngineError::Unimplemented {
            method: "reopen_closed_tab",
        })
    }

    fn closed_stack_len(&self) -> usize {
        0
    }

    fn active_tab(&self) -> Option<TabSummary> {
        self.with_state(|st| {
            st.active_idx
                .and_then(|i| st.tabs.get(i))
                .map(|t| t.to_summary())
        })
    }

    fn tabs_summary(&self) -> Vec<TabSummary> {
        self.with_state(|st| st.summaries())
    }

    fn tab_count(&self) -> usize {
        self.with_state(|st| st.tabs.len())
    }

    fn pinned_count(&self) -> usize {
        0
    }

    fn active_index(&self) -> Option<usize> {
        self.with_state(|st| st.active_idx)
    }

    // ── Navigation ───────────────────────────────────────────────────────────

    fn navigate(&self, url: &str) -> Result<(), EngineError> {
        tracing::debug!("webview2: navigate {url}");
        self.worker.send(Command::Navigate {
            url: url.to_owned(),
        });
        Ok(())
    }

    fn active_tab_live_url(&self) -> String {
        self.with_state(|st| {
            st.active_idx
                .and_then(|i| st.tabs.get(i))
                .map(|t| t.url.clone())
                .unwrap_or_default()
        })
    }

    fn pump_address_changes(&self) -> bool {
        true
    }

    fn can_go_back(&self) -> bool {
        self.worker
            .call(|reply| Command::QueryCanGoBack { reply })
            .unwrap_or(false)
    }

    fn can_go_forward(&self) -> bool {
        self.worker
            .call(|reply| Command::QueryCanGoForward { reply })
            .unwrap_or(false)
    }

    // ── Viewport ─────────────────────────────────────────────────────────────

    fn resize(&self, width: u32, height: u32) {
        self.osr_resize(width, height);
    }

    fn set_device_scale(&self, scale: f32) {
        tracing::debug!("webview2: set_device_scale {scale}");
        self.view.set_scale(scale);
    }

    fn set_frame_rate(&self, _hz: u32) {
        tracing::debug!("webview2: set_frame_rate — no-op (event-driven)");
    }

    fn notify_screen_info_changed(&self) {
        tracing::debug!("webview2: notify_screen_info_changed — no-op");
    }

    fn osr_resize(&self, width: u32, height: u32) {
        tracing::debug!("webview2: osr_resize {width}×{height}");
        if let Ok(mut guard) = self.frame.lock() {
            guard.needs_fresh = true;
        }
        self.view.width.store(width, Ordering::Relaxed);
        self.view.height.store(height, Ordering::Relaxed);
        self.worker.send(Command::Resize { width, height });
    }

    // ── Input ────────────────────────────────────────────────────────────────
    //
    // Input synthesis requires calling `SendKeyboardInput` / `SendMouseInput`
    // on `ICoreWebView2CompositionController` from the STA thread. Phase B
    // logs the translated event. Phase C will dispatch via
    // `Command::SendKeyboard` / `Command::SendMouse`.

    fn osr_key_event(&self, event: NeutralKeyEvent) {
        let ev = neutral_key_to_wv2(&event);
        tracing::debug!(
            "webview2: osr_key_event {} — TODO(input-key): dispatch to CompositionController",
            ev.description
        );
        // TODO(input-key): self.worker.send(Command::SendKeyboard { ev });
    }

    fn osr_mouse_move(&self, x: i32, y: i32, _modifiers: u32) {
        let ev = neutral_move_to_wv2(x, y);
        tracing::debug!(
            "webview2: osr_mouse_move ({},{}) — TODO(input-mouse): dispatch",
            ev.x,
            ev.y
        );
        // TODO(input-mouse): self.worker.send(Command::SendMouse { ev });
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
        let ev = neutral_click_to_wv2(x, y, button, mouse_up);
        tracing::debug!(
            "webview2: osr_mouse_click ({},{}) up={mouse_up} — TODO(input-mouse): dispatch",
            ev.x,
            ev.y
        );
        // TODO(input-mouse): self.worker.send(Command::SendMouse { ev });
    }

    fn osr_mouse_leave(&self, _modifiers: u32) {
        let _ev = neutral_leave_to_wv2();
        tracing::debug!("webview2: osr_mouse_leave — TODO(input-mouse): dispatch");
        self.worker.send(Command::ForcePaint);
    }

    fn osr_mouse_wheel(&self, x: i32, y: i32, delta_x: i32, delta_y: i32, _modifiers: u32) {
        let ev = neutral_scroll_to_wv2(x, y, delta_x, delta_y);
        tracing::debug!(
            "webview2: osr_mouse_wheel ({},{}) — TODO(input-mouse): dispatch",
            ev.x,
            ev.y
        );
        // TODO(input-mouse): self.worker.send(Command::SendMouse { ev });
    }

    fn osr_focus(&self, focused: bool) {
        tracing::debug!("webview2: osr_focus({focused}) — no-op in Phase B");
    }

    // ── OSR state ────────────────────────────────────────────────────────────

    fn osr_frame(&self) -> SharedOsrFrame {
        Arc::clone(&self.frame)
    }

    fn osr_view(&self) -> SharedOsrViewState {
        Arc::clone(&self.view)
    }

    fn force_repaint_active(&self) {
        tracing::debug!("webview2: force_repaint_active");
        self.worker.send(Command::ForcePaint);
    }

    fn osr_sleep(&self, sleep: bool) {
        tracing::debug!("webview2: osr_sleep({sleep}) — no-op");
    }

    fn osr_invalidate_view(&self) {
        tracing::debug!("webview2: osr_invalidate_view");
        self.worker.send(Command::ForcePaint);
    }

    fn set_osr_wake(&self, wake: Arc<dyn Fn() + Send + Sync>) {
        self.view.set_wake(wake);
        tracing::debug!("webview2: wake callback installed");
    }

    // ── Find / zoom ──────────────────────────────────────────────────────────

    fn start_find(&self, _query: &str, _forward: bool) {
        tracing::debug!("webview2: start_find — no-op in Phase B");
    }

    fn stop_find(&self) {
        tracing::debug!("webview2: stop_find — no-op in Phase B");
    }

    fn active_zoom_level(&self) -> f64 {
        1.0
    }

    // ── Audio / video ────────────────────────────────────────────────────────

    fn any_audio_active(&self) -> bool {
        false
    }

    fn any_video_active(&self) -> bool {
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

    fn popup_resize(&self, _browser_id: i32, _width: u32, _height: u32) {}
    fn popup_close(&self, _browser_id: i32) {}
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
        tracing::debug!("webview2: cancel_hint — no-op");
    }
}
