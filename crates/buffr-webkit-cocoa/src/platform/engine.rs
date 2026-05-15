//! [`WebKitCocoaEngine`] — real [`BrowserEngine`] impl for the WebKit Cocoa backend.
//!
//! # Phase B status
//!
//! ## Architecture
//!
//! WKWebView is a main-thread-only object.  `WebKitCocoaEngine` is `Send + Sync`
//! (required by `BrowserEngine`). The engine holds only thread-safe handles:
//!
//! ```text
//! WebKitCocoaEngine (any thread — Send + Sync)
//!   cmd_tx ──── Command ────▶  worker thread
//!                                └─ dispatch_async(main_queue) ──▶ WKWebView
//!   engine_state ────────────────────────────────────▶ Arc<Mutex<EngineState>>
//!                (written by main-queue delegate callbacks; read by any thread)
//! ```
//!
//! ## Implemented (macOS — code-review-quality, not build-verified on Linux)
//!
//! - `close_all_browsers` / `open_tab` / `open_tab_background` / `open_tab_at`
//! - `close_tab` / `close_active`
//! - `select_tab` / `next_tab` / `prev_tab`
//! - `navigate`
//! - `can_go_back` / `can_go_forward`
//!
//! `go_back` / `go_forward` / `reload` / `stop` are NOT on the `BrowserEngine`
//! trait; they live as inherent methods. Phase C wires them in via
//! `dispatch(&PageAction)`.
//! - `active_tab` / `tabs_summary` / `tab_count` / `active_index`
//! - `active_tab_live_url`
//! - `osr_resize` / `osr_frame` / `osr_view` / `set_osr_wake`
//! - `force_repaint_active` / `osr_invalidate_view`
//! - `osr_key_event` / `osr_mouse_move` / `osr_mouse_click` / `osr_mouse_leave`
//! - `osr_mouse_wheel`
//!
//! ## Delegated to stubs (unchanged from Phase A)
//!
//! popup, hint, find, zoom, downloads, devtools, scheme, edit, clipboard.

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

use super::error::WebKitCocoaError;

// On macOS: import the real worker. On Linux/Windows: nothing to import
// (the platform module doesn't compile there).
#[cfg(target_os = "macos")]
use super::worker::macos::{Command, EngineState, WorkerHandle};

// ── Engine ────────────────────────────────────────────────────────────────────

/// WebKit Cocoa browser engine.
///
/// Implements [`BrowserEngine`] (`Send + Sync`) by forwarding all WKWebView
/// operations to the macOS main queue via a background worker thread.
pub struct WebKitCocoaEngine {
    #[allow(dead_code)]
    engine_id: EngineId,
    /// Shared OSR frame — written by main-queue snapshot completions.
    frame: SharedOsrFrame,
    /// Shared viewport state.
    view: SharedOsrViewState,
    /// Worker channel. On macOS: real worker. Wrapped in Option so we can
    /// compile the struct on all targets (the field is always Some on macOS).
    #[cfg(target_os = "macos")]
    worker: WorkerHandle,
    /// Thread-safe tab snapshot. Updated by delegate callbacks on the main queue.
    #[cfg(target_os = "macos")]
    engine_state: Arc<Mutex<EngineState>>,
}

impl WebKitCocoaEngine {
    pub fn new(options: &BackendOpenOptions<'_>) -> Result<Self, WebKitCocoaError> {
        let (width, height) = options.initial_size;

        let frame: SharedOsrFrame = Arc::new(Mutex::new(OsrFrame::new(width, height)));
        let view: SharedOsrViewState = {
            let v = OsrViewState::new();
            v.width.store(width, Ordering::Relaxed);
            v.height.store(height, Ordering::Relaxed);
            Arc::new(v)
        };

        #[cfg(target_os = "macos")]
        {
            let engine_state = Arc::new(Mutex::new(EngineState::new()));

            let worker = super::worker::macos::spawn(
                options.initial_url,
                width,
                height,
                Arc::clone(&frame),
                Arc::clone(&view),
                Arc::clone(&engine_state),
            )?;

            tracing::info!(
                "webkit-cocoa engine created (id={}, url={}, size={width}×{height})",
                options.engine_id.as_str(),
                options.initial_url,
            );

            return Ok(WebKitCocoaEngine {
                engine_id: options.engine_id.clone(),
                frame,
                view,
                worker,
                engine_state,
            });
        }

        // Non-macOS: only compiled and reachable on non-macOS targets.
        // The `#[cfg(target_os = "macos")]` block above returns early on macOS.
        #[cfg(not(target_os = "macos"))]
        Ok(WebKitCocoaEngine {
            engine_id: options.engine_id.clone(),
            frame,
            view,
        })
    }

    /// Read a snapshot from `engine_state`. Non-blocking, safe from any thread.
    #[cfg(target_os = "macos")]
    fn with_state<T>(&self, f: impl FnOnce(&EngineState) -> T) -> T {
        match self.engine_state.lock() {
            Ok(g) => f(&g),
            Err(_) => f(&EngineState::new()),
        }
    }
}

impl Drop for WebKitCocoaEngine {
    fn drop(&mut self) {
        tracing::info!("webkit-cocoa engine: shutting down worker");
        #[cfg(target_os = "macos")]
        self.worker.send(Command::Shutdown);
    }
}

// ── BrowserEngine impl ────────────────────────────────────────────────────────

impl BrowserEngine for WebKitCocoaEngine {
    // ── Lifecycle ────────────────────────────────────────────────────────────

    fn close_all_browsers(&self) {
        tracing::debug!("webkit-cocoa: close_all_browsers");
        #[cfg(target_os = "macos")]
        self.worker.send(Command::CloseAll);
    }

    // ── Tabs ─────────────────────────────────────────────────────────────────

    fn open_tab(&self, url: &str) -> Result<TabId, EngineError> {
        tracing::debug!("webkit-cocoa: open_tab {url}");
        #[cfg(target_os = "macos")]
        return self
            .worker
            .call(|reply| Command::OpenTab {
                url: url.to_owned(),
                reply,
            })
            .map_err(EngineError::from)?
            .map_err(EngineError::from);

        #[cfg(not(target_os = "macos"))]
        Err(EngineError::Unimplemented { method: "open_tab" })
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
        tracing::debug!("webkit-cocoa: open_tab_at {url} (insert_idx ignored in Phase B)");
        self.open_tab(url)
    }

    fn close_tab(&self, id: TabId) -> Result<bool, EngineError> {
        tracing::debug!("webkit-cocoa: close_tab {id:?}");
        #[cfg(target_os = "macos")]
        return self
            .worker
            .call(|reply| Command::CloseTab { id, reply })
            .map_err(EngineError::from)?
            .map_err(EngineError::from);

        #[cfg(not(target_os = "macos"))]
        Err(EngineError::Unimplemented {
            method: "close_tab",
        })
    }

    fn close_active(&self) -> Result<bool, EngineError> {
        let active = self.active_tab().map(|t| t.id);
        if let Some(id) = active {
            self.close_tab(id)
        } else {
            Ok(false)
        }
    }

    fn select_tab(&self, id: TabId) {
        tracing::debug!("webkit-cocoa: select_tab {id:?}");
        #[cfg(target_os = "macos")]
        self.worker.send(Command::SelectTab { id });
    }

    fn next_tab(&self) {
        #[cfg(target_os = "macos")]
        self.worker.send(Command::CycleTab { forward: true });
    }

    fn prev_tab(&self) {
        #[cfg(target_os = "macos")]
        self.worker.send(Command::CycleTab { forward: false });
    }

    fn move_tab(&self, _from: usize, _to: usize) {
        tracing::debug!("webkit-cocoa: move_tab not implemented in Phase B");
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
        #[cfg(target_os = "macos")]
        return self.with_state(|st| {
            st.active_idx
                .and_then(|i| st.summaries().into_iter().nth(i))
        });

        #[cfg(not(target_os = "macos"))]
        None
    }

    fn tabs_summary(&self) -> Vec<TabSummary> {
        #[cfg(target_os = "macos")]
        return self.with_state(|st| st.summaries());

        #[cfg(not(target_os = "macos"))]
        vec![]
    }

    fn tab_count(&self) -> usize {
        #[cfg(target_os = "macos")]
        return self.with_state(|st| st.tabs.len());

        #[cfg(not(target_os = "macos"))]
        0
    }

    fn pinned_count(&self) -> usize {
        0
    }

    fn active_index(&self) -> Option<usize> {
        #[cfg(target_os = "macos")]
        return self.with_state(|st| st.active_idx);

        #[cfg(not(target_os = "macos"))]
        None
    }

    // ── Navigation ───────────────────────────────────────────────────────────

    fn navigate(&self, url: &str) -> Result<(), EngineError> {
        tracing::debug!("webkit-cocoa: navigate {url}");
        #[cfg(target_os = "macos")]
        self.worker.send(Command::Navigate {
            url: url.to_owned(),
        });
        Ok(())
    }

    fn active_tab_live_url(&self) -> String {
        #[cfg(target_os = "macos")]
        return self.with_state(|st| {
            st.active_idx
                .and_then(|i| st.tabs.get(i))
                .map(|t| t.url.clone())
                .unwrap_or_default()
        });

        #[cfg(not(target_os = "macos"))]
        String::new()
    }

    fn pump_address_changes(&self) -> bool {
        true
    }

    fn can_go_back(&self) -> bool {
        #[cfg(target_os = "macos")]
        return self
            .worker
            .call(|reply| Command::QueryCanGoBack { reply })
            .unwrap_or(false);

        #[cfg(not(target_os = "macos"))]
        false
    }

    fn can_go_forward(&self) -> bool {
        #[cfg(target_os = "macos")]
        return self
            .worker
            .call(|reply| Command::QueryCanGoForward { reply })
            .unwrap_or(false);

        #[cfg(not(target_os = "macos"))]
        false
    }

    // ── Viewport ─────────────────────────────────────────────────────────────

    fn resize(&self, width: u32, height: u32) {
        self.osr_resize(width, height);
    }

    fn set_device_scale(&self, scale: f32) {
        tracing::debug!("webkit-cocoa: set_device_scale {scale}");
        self.view.set_scale(scale);
    }

    fn set_frame_rate(&self, _hz: u32) {
        tracing::debug!("webkit-cocoa: set_frame_rate — no-op (event-driven)");
    }

    fn notify_screen_info_changed(&self) {
        tracing::debug!("webkit-cocoa: notify_screen_info_changed — no-op");
    }

    fn osr_resize(&self, width: u32, height: u32) {
        tracing::debug!("webkit-cocoa: osr_resize {width}×{height}");
        if let Ok(mut guard) = self.frame.lock() {
            guard.needs_fresh = true;
        }
        self.view.width.store(width, Ordering::Relaxed);
        self.view.height.store(height, Ordering::Relaxed);
        #[cfg(target_os = "macos")]
        self.worker.send(Command::Resize { width, height });
    }

    // ── Input ────────────────────────────────────────────────────────────────

    fn osr_key_event(&self, event: NeutralKeyEvent) {
        tracing::debug!("webkit-cocoa: osr_key_event kind={:?}", event.kind);
        #[cfg(target_os = "macos")]
        self.worker.send(Command::KeyEvent { event });
    }

    fn osr_mouse_move(&self, x: i32, y: i32, modifiers: u32) {
        tracing::debug!("webkit-cocoa: osr_mouse_move ({x},{y})");
        #[cfg(target_os = "macos")]
        self.worker.send(Command::MouseMove { x, y, modifiers });
    }

    fn osr_mouse_click(
        &self,
        x: i32,
        y: i32,
        button: MouseButton,
        mouse_up: bool,
        click_count: i32,
        modifiers: u32,
    ) {
        tracing::debug!("webkit-cocoa: osr_mouse_click ({x},{y}) up={mouse_up}");
        #[cfg(target_os = "macos")]
        self.worker.send(Command::MouseClick {
            x,
            y,
            button,
            mouse_up,
            click_count,
            modifiers,
        });
    }

    fn osr_mouse_leave(&self, _modifiers: u32) {
        tracing::debug!("webkit-cocoa: osr_mouse_leave");
        // No dedicated NSEvent type for mouse-leave in AppKit OSR.
        // Force a repaint so the view updates its hover state.
        #[cfg(target_os = "macos")]
        self.worker.send(Command::ForcePaint);
    }

    fn osr_mouse_wheel(&self, x: i32, y: i32, delta_x: i32, delta_y: i32, modifiers: u32) {
        tracing::debug!("webkit-cocoa: osr_mouse_wheel ({x},{y}) dx={delta_x} dy={delta_y}");
        #[cfg(target_os = "macos")]
        self.worker.send(Command::MouseWheel {
            x,
            y,
            delta_x,
            delta_y,
            modifiers,
        });
    }

    fn osr_focus(&self, focused: bool) {
        tracing::debug!("webkit-cocoa: osr_focus({focused}) — no-op in Phase B");
    }

    // ── OSR state ────────────────────────────────────────────────────────────

    fn osr_frame(&self) -> SharedOsrFrame {
        Arc::clone(&self.frame)
    }

    fn osr_view(&self) -> SharedOsrViewState {
        Arc::clone(&self.view)
    }

    fn force_repaint_active(&self) {
        tracing::debug!("webkit-cocoa: force_repaint_active");
        #[cfg(target_os = "macos")]
        self.worker.send(Command::ForcePaint);
    }

    fn osr_sleep(&self, sleep: bool) {
        tracing::debug!("webkit-cocoa: osr_sleep({sleep}) — no-op");
    }

    fn osr_invalidate_view(&self) {
        tracing::debug!("webkit-cocoa: osr_invalidate_view");
        #[cfg(target_os = "macos")]
        self.worker.send(Command::ForcePaint);
    }

    fn set_osr_wake(&self, wake: Arc<dyn Fn() + Send + Sync>) {
        self.view.set_wake(wake);
        tracing::debug!("webkit-cocoa: wake callback installed");
    }

    // ── Find / zoom ──────────────────────────────────────────────────────────

    fn start_find(&self, _query: &str, _forward: bool) {
        tracing::debug!("webkit-cocoa: start_find — no-op in Phase B");
    }

    fn stop_find(&self) {
        tracing::debug!("webkit-cocoa: stop_find — no-op in Phase B");
    }

    fn active_zoom_level(&self) -> f64 {
        // Read from EngineState cache — no main-queue round-trip needed.
        // The cache is refreshed by `RuntimeState::snapshot_to_engine_state()`
        // after every `Command::SetZoom` dispatch.
        #[cfg(target_os = "macos")]
        {
            if let Ok(guard) = self.engine_state.lock()
                && let Some(idx) = guard.active_idx
                && let Some(tab) = guard.tabs.get(idx)
            {
                return tab.zoom;
            }
        }
        1.0
    }

    fn zoom_in(&self) {
        let next = (self.active_zoom_level() + 0.1).min(5.0);
        #[cfg(target_os = "macos")]
        self.worker.send(Command::SetZoom(next));
        #[cfg(not(target_os = "macos"))]
        let _ = next;
    }

    fn zoom_out(&self) {
        let next = (self.active_zoom_level() - 0.1).max(0.25);
        #[cfg(target_os = "macos")]
        self.worker.send(Command::SetZoom(next));
        #[cfg(not(target_os = "macos"))]
        let _ = next;
    }

    fn zoom_reset(&self) {
        #[cfg(target_os = "macos")]
        self.worker.send(Command::SetZoom(1.0));
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
        tracing::debug!("webkit-cocoa: cancel_hint — no-op");
    }
}

/// History / loading controls — NOT on the `BrowserEngine` trait.
///
/// These live as inherent methods because the trait surface routes navigation
/// through `dispatch(&PageAction)`. Phase C can override `dispatch()` to call
/// these via `PageAction::Back` / `Forward` / `Reload` / `StopLoading`. The
/// corresponding `Command::GoBack` / `GoForward` / `Reload` / `Stop` worker
/// commands are wired and ready.
#[cfg(target_os = "macos")]
impl WebKitCocoaEngine {
    pub fn go_back(&self) -> bool {
        self.worker.send(Command::GoBack);
        true
    }

    pub fn go_forward(&self) -> bool {
        self.worker.send(Command::GoForward);
        true
    }

    pub fn reload(&self) {
        self.worker.send(Command::Reload);
    }

    pub fn stop(&self) {
        self.worker.send(Command::Stop);
    }
}
