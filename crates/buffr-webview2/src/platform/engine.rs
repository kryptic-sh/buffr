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

use std::sync::atomic::{AtomicBool, Ordering};
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
    WebView2InputEvent, Wv2ImeEvent, Wv2MouseEvent, Wv2MouseKind, neutral_click_to_wv2,
    neutral_key_to_wv2, neutral_leave_to_wv2, neutral_move_to_wv2, neutral_scroll_to_wv2,
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
    /// Cached "any tab playing audio?" flag.  Written by the 500 ms STA-thread
    /// `WM_TIMER` handler; read from any thread by `any_audio_active()`.
    audio_active: Arc<AtomicBool>,
    /// Last find query stored by `start_find`; re-used by `dispatch` for
    /// `FindNext` / `FindPrev`. Cleared by `stop_find`.
    find_query: Mutex<Option<String>>,
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

        // Clone the Arc<AtomicBool> from inside EngineState so the engine can
        // read the audio flag from any thread without locking the Mutex.
        let audio_active = engine_state
            .lock()
            .map(|g| Arc::clone(&g.audio_active))
            .unwrap_or_else(|_| Arc::new(AtomicBool::new(false)));

        let data_dir = options.data_dir.map(|p| p.to_path_buf());

        let worker = spawn(
            options.initial_url,
            width,
            height,
            Arc::clone(&frame),
            Arc::clone(&view),
            Arc::clone(&engine_state),
            data_dir,
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
            audio_active,
            find_query: Mutex::new(None),
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
        self.worker
            .call(|reply| Command::Navigate {
                url: url.to_owned(),
                reply,
            })
            .map_err(EngineError::from)?
            .map_err(EngineError::from)
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
        tracing::debug!("webview2: osr_key_event — routing to STA worker");
        self.worker.send(Command::SendInput(ev));
    }

    fn osr_mouse_move(&self, x: i32, y: i32, _modifiers: u32) {
        let ev = neutral_move_to_wv2(x, y);
        tracing::debug!("webview2: osr_mouse_move ({x},{y}) — routing to STA worker");
        self.worker.send(Command::SendInput(ev));
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
            "webview2: osr_mouse_click ({x},{y}) up={mouse_up} — routing to STA worker"
        );
        self.worker.send(Command::SendInput(ev));
    }

    fn osr_mouse_leave(&self, _modifiers: u32) {
        let ev = neutral_leave_to_wv2();
        tracing::debug!("webview2: osr_mouse_leave — routing to STA worker");
        self.worker.send(Command::SendInput(ev));
    }

    fn osr_mouse_wheel(&self, x: i32, y: i32, delta_x: i32, delta_y: i32, _modifiers: u32) {
        let ev = neutral_scroll_to_wv2(x, y, delta_x, delta_y);
        tracing::debug!("webview2: osr_mouse_wheel ({x},{y}) — routing to STA worker");
        self.worker.send(Command::SendInput(ev));
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

    fn start_find(&self, query: &str, _forward: bool) {
        if let Ok(mut g) = self.find_query.lock() {
            *g = Some(query.to_owned());
        }
        tracing::debug!("webview2: start_find — no native find API in Phase B (query stored)");
    }

    fn stop_find(&self) {
        if let Ok(mut g) = self.find_query.lock() {
            *g = None;
        }
        tracing::debug!("webview2: stop_find");
    }

    fn active_zoom_level(&self) -> f64 {
        // Read from the EngineState cache; no STA round-trip needed.
        // The cache is updated by `Command::SetZoom` on the STA thread after
        // every `ICoreWebView2Controller::SetZoomFactor` call.
        if let Ok(guard) = self.engine_state.lock()
            && let Some(idx) = guard.active_idx
            && let Some(tab) = guard.tabs.get(idx)
        {
            return tab.zoom;
        }
        1.0
    }

    // ── JS execution (Phase 6c, #95) ─────────────────────────────────────────
    //
    // Fire-and-forget JS evaluation via the STA worker →
    // `ICoreWebView2::ExecuteScript`. The completion handler is a no-op.
    // The `url` origin parameter is not exposed by the WebView2 ExecuteScript
    // API — the document origin is derived from the currently loaded page.
    //
    // TODO(phase-c): use `ICoreWebView2_10::ExecuteScriptWithResult` to surface
    //   evaluation errors via `tracing::warn!`.

    fn run_js(&self, code: &str) -> Result<(), EngineError> {
        tracing::debug!("webview2: run_js ({} bytes)", code.len());
        self.worker.send(Command::EvalJs {
            code: code.to_owned(),
        });
        Ok(())
    }

    fn run_main_frame_js(&self, code: &str, _url: &str) -> Result<(), EngineError> {
        self.run_js(code)
    }

    // ── Downloads (Phase 6c, #95) ────────────────────────────────────────────

    fn start_download(&self, url: &str) {
        tracing::debug!("webview2: start_download url={url}");
        let url_json = serde_json::to_string(url).unwrap_or_else(|_| "\"\"".into());
        let js = format!(
            "(() => {{ const a = document.createElement('a'); \
             a.href = {url_json}; a.download = ''; \
             document.body.appendChild(a); a.click(); a.remove(); }})();"
        );
        let _ = self.run_js(&js);
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

    // ── DevTools ─────────────────────────────────────────────────────────────

    fn open_devtools(&self, tab: TabId) -> Result<(), EngineError> {
        tracing::debug!("webview2: open_devtools {tab:?}");
        self.worker
            .call(|reply| Command::OpenDevtools { id: tab, reply })
            .map_err(EngineError::from)?
            .map_err(EngineError::from)
    }

    fn show_dev_tools_at(&self, _x: i32, _y: i32) {
        if let Some(tab) = self.active_tab() {
            let _ = self.open_devtools(tab.id);
        }
    }

    // ── Audio / video ────────────────────────────────────────────────────────

    fn any_audio_active(&self) -> bool {
        self.audio_active.load(Ordering::Relaxed)
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

    // ── Frame editing (Phase 6c, #95) ────────────────────────────────────────

    fn frame_undo(&self) {
        let _ = self.run_js("document.execCommand('undo')");
    }

    fn frame_redo(&self) {
        let _ = self.run_js("document.execCommand('redo')");
    }

    fn frame_cut(&self) {
        let _ = self.run_js("document.execCommand('cut')");
    }

    fn frame_copy(&self) {
        let _ = self.run_js("document.execCommand('copy')");
    }

    fn frame_paste(&self) {
        let _ = self.run_js("document.execCommand('paste')");
    }

    fn frame_paste_plain(&self) {
        let _ = self.run_js("navigator.clipboard.readText().then(t => document.execCommand('insertText', false, t))");
    }

    fn frame_select_all(&self) {
        let _ = self.run_js("document.execCommand('selectAll')");
    }

    // ── IME composition (Phase 8d, #86) ──────────────────────────────────────
    //
    // On Windows, real IME delivery goes through WM_IME_STARTCOMPOSITION /
    // WM_IME_COMPOSITION / WM_IME_ENDCOMPOSITION with the composition string
    // stored in an IME-managed buffer (read via ImmGetCompositionString).
    //
    // Phase B simplification: we send WM_CHAR per character via PostMessageW,
    // which mimics the final keystroke sequence a real IME would produce.
    // This is the same approach used by AutoHotkey, Playwright for Electron,
    // and other Win32 automation tools when IME simulation is needed.
    //
    // ime_set_composition and ime_cancel use the same WM_CHAR path:
    //   - set_composition → WM_CHAR per preedit character (no OS preedit popup)
    //   - cancel → WM_CHAR(VK_ESCAPE)
    //
    // Phase D: replace with WM_IME_STARTCOMPOSITION → WM_IME_COMPOSITION
    //   (GCS_COMPSTR | GCS_CURSORPOS lParam) → WM_IME_ENDCOMPOSITION and use
    //   ImmSetCompositionString to plant the preedit in the IME buffer first.
    //
    // On non-Windows targets: debug-log only (the STA worker stub ignores these).

    fn ime_set_composition(&self, text: &str, cursor: Option<(usize, usize)>) {
        tracing::debug!(
            text,
            ?cursor,
            "webview2: ime_set_composition (WM_CHAR simplification; Phase D deferred)"
        );
        self.worker.send(Command::SendInput(WebView2InputEvent::Ime(
            Wv2ImeEvent::Preedit {
                text: text.to_owned(),
                cursor,
            },
        )));
    }

    fn ime_commit(&self, text: &str) {
        tracing::debug!(text, "webview2: ime_commit via WM_CHAR");
        self.worker.send(Command::SendInput(WebView2InputEvent::Ime(
            Wv2ImeEvent::Commit {
                text: text.to_owned(),
            },
        )));
    }

    fn ime_cancel(&self) {
        tracing::debug!("webview2: ime_cancel via WM_CHAR(VK_ESCAPE)");
        self.worker.send(Command::SendInput(WebView2InputEvent::Ime(
            Wv2ImeEvent::Cancel,
        )));
    }

    // ── Action dispatch ───────────────────────────────────────────────────────
    //
    // History/stop → existing GoBack/GoForward/Reload/Stop worker commands.
    // Scroll → SendInput(WebView2InputEvent::Mouse(Wheel{delta})).
    //   Windows WHEEL_DELTA = 120 per notch; we use 3 notches per STEP_PX unit.
    // Zoom → existing zoom_* helpers.
    // FindNext/FindPrev → debug-log (no native find API in Phase B).

    fn dispatch(&self, action: &buffr_modal::PageAction) {
        use buffr_modal::PageAction as A;

        /// Pixels per scroll step (matches blink-cdp / CEF constant).
        const STEP_PX: i32 = 40;
        /// Win32 WHEEL_DELTA per notch. Negative = scroll down (away from user).
        const WHEEL_DELTA: i32 = 120;

        // Helper: send a vertical wheel event with `delta` (positive = up).
        let wheel = |delta: i32| {
            self.worker
                .send(Command::SendInput(WebView2InputEvent::Mouse(
                    Wv2MouseEvent {
                        x: 0,
                        y: 0,
                        kind: Wv2MouseKind::Wheel { delta },
                    },
                )));
        };

        match action {
            // ── Find ─────────────────────────────────────────────────────────
            A::FindNext => {
                let query = self.find_query.lock().ok().and_then(|g| g.clone());
                if let Some(q) = query {
                    tracing::debug!(query = %q, "webview2: dispatch FindNext — no native find API (no-op)");
                } else {
                    tracing::debug!("webview2: FindNext — no active find query");
                }
            }
            A::FindPrev => {
                let query = self.find_query.lock().ok().and_then(|g| g.clone());
                if let Some(q) = query {
                    tracing::debug!(query = %q, "webview2: dispatch FindPrev — no native find API (no-op)");
                } else {
                    tracing::debug!("webview2: FindPrev — no active find query");
                }
            }

            // ── History / reload / stop ───────────────────────────────────────
            A::HistoryBack => {
                tracing::debug!("webview2: dispatch HistoryBack");
                self.worker.send(Command::GoBack);
            }
            A::HistoryForward => {
                tracing::debug!("webview2: dispatch HistoryForward");
                self.worker.send(Command::GoForward);
            }
            A::Reload | A::ReloadHard => {
                tracing::debug!("webview2: dispatch Reload");
                self.worker.send(Command::Reload);
            }
            A::StopLoading => {
                tracing::debug!("webview2: dispatch StopLoading");
                self.worker.send(Command::Stop);
            }

            // ── Scroll via synthetic wheel event ──────────────────────────────
            A::ScrollUp(n) => {
                let delta = WHEEL_DELTA * (*n as i32) * STEP_PX / 40;
                tracing::debug!(n, delta, "webview2: dispatch ScrollUp");
                wheel(delta);
            }
            A::ScrollDown(n) => {
                let delta = -(WHEEL_DELTA * (*n as i32) * STEP_PX / 40);
                tracing::debug!(n, delta, "webview2: dispatch ScrollDown");
                wheel(delta);
            }
            A::ScrollLeft(_) | A::ScrollRight(_) => {
                tracing::debug!(action = ?action, "webview2: dispatch ScrollLeft/Right — no-op");
            }
            A::ScrollPageDown | A::ScrollFullPageDown => {
                tracing::debug!("webview2: dispatch ScrollPageDown");
                wheel(-(WHEEL_DELTA * 5));
            }
            A::ScrollPageUp | A::ScrollFullPageUp => {
                tracing::debug!("webview2: dispatch ScrollPageUp");
                wheel(WHEEL_DELTA * 5);
            }
            A::ScrollHalfPageDown => {
                tracing::debug!("webview2: dispatch ScrollHalfPageDown");
                wheel(-(WHEEL_DELTA * 3));
            }
            A::ScrollHalfPageUp => {
                tracing::debug!("webview2: dispatch ScrollHalfPageUp");
                wheel(WHEEL_DELTA * 3);
            }
            A::ScrollTop | A::ScrollBottom => {
                tracing::debug!(
                    action = ?action,
                    "webview2: dispatch ScrollTop/Bottom — no-op (no absolute scroll without ExecuteScript)"
                );
            }

            // ── Zoom ──────────────────────────────────────────────────────────
            A::ZoomIn => self.zoom_in(),
            A::ZoomOut => self.zoom_out(),
            A::ZoomReset => self.zoom_reset(),

            other => {
                tracing::debug!(
                    action = ?other,
                    "webview2: dispatch — action not handled by this backend (no-op)"
                );
            }
        }
    }
}
