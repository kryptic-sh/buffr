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
//! popup, find, zoom, downloads, devtools, scheme, edit, clipboard.
//! Input synthesis is logged but not yet dispatched to the composition
//! controller — see `input.rs` TODO markers.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use buffr_core::find::FindResultSink;
use buffr_core::hint::{
    DEFAULT_HINT_SELECTORS, HintAlphabet, HintEventSink, HintSession, build_inject_script,
    new_hint_event_sink, take_hint_event,
};
use buffr_engine::{
    BackendOpenOptions, BrowserEngine, EngineError, FaviconUpdate, HintAction, HintStatus,
    MouseButton, NeutralKeyEvent, OsrFrame, OsrViewState, SharedOsrFrame, SharedOsrViewState,
    TabId, TabSummary,
    engine_id::EngineId,
    popup::{
        PopupCloseSink, PopupCreateSink, PopupQueue, new_popup_close_sink, new_popup_create_sink,
    },
};

use super::error::WebView2Error;
use super::input::{
    WebView2InputEvent, Wv2ImeEvent, Wv2MouseEvent, Wv2MouseKind, neutral_click_to_wv2,
    neutral_key_to_wv2, neutral_leave_to_wv2, neutral_move_to_wv2, neutral_scroll_to_wv2,
};
use super::worker::{Command, EngineState, WorkerHandle, spawn};

// ── Cursor CSS → raw kind mapping ────────────────────────────────────────────

/// Map a CSS cursor name to a CEF-compatible raw cursor kind integer.
///
/// Values match `cef_cursor_type_t` from the CEF API:
/// <https://bitbucket.org/chromiumembedded/cef/src/master/include/internal/cef_types.h>
///
/// Used by `take_cursor_change` callers that share the same chrome-strip
/// cursor-icon lookup table across all backends.
pub(crate) fn css_cursor_to_cef_raw(name: &str) -> u32 {
    match name {
        "text" | "vertical-text" => 1,
        "pointer" => 28,
        "default" | "auto" | "" => 0,
        "wait" | "progress" => 13,
        "crosshair" => 6,
        "move" | "all-scroll" => 9,
        "not-allowed" | "no-drop" => 12,
        "grab" => 34,
        "grabbing" => 35,
        "help" => 8,
        _ => 0,
    }
}

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
    /// Set by `run_media_probe` when the active page has a playing `<video>` or
    /// `<audio>` element.  Written by the `WebMessageReceived` handler when the
    /// `__buffr_media__:true/false` sentinel is received; read lock-free from
    /// any thread by `any_video_active()`.
    video_active: Arc<AtomicBool>,
    /// One-slot mailbox for cursor changes. Written by the `WebMessageReceived`
    /// handler on the STA thread when `__buffr_cursor__:<css-cursor>` is
    /// received; drained from any thread by `take_cursor_change`.
    /// Stores `(browser_id, raw_kind)` — `browser_id` is `tab_id.0 as i32`.
    cursor_change: Arc<Mutex<Option<(i32, u32)>>>,
    /// System clipboard handle.  `None` when `hjkl_clipboard::Clipboard::new()`
    /// fails (e.g. no display server on CI).  Clipboard operations fall back to
    /// the STA worker Win32 path when `None`.
    clipboard: Option<std::sync::Arc<hjkl_clipboard::Clipboard>>,
    /// Cached loading state of the **active** tab.
    ///
    /// Written by `NavigationStarting` / `NavigationCompleted` event handlers
    /// and `sync_active_idx` (both on the STA thread) via
    /// `EngineState::sync_loading_active`.  Read lock-free from any thread by
    /// `BrowserEngine::is_loading`.
    loading_active: Arc<AtomicBool>,
    /// Last find query stored by `start_find`; re-used by `dispatch` for
    /// `FindNext` / `FindPrev`. Cleared by `stop_find`.
    find_query: Mutex<Option<String>>,
    /// Shared favicon update queue.  Favicon events are pushed by the STA
    /// thread's `FaviconChanged` handler; drained from any thread by
    /// `BrowserEngine::drain_favicon_updates`.
    favicon_updates: Arc<Mutex<Vec<FaviconUpdate>>>,
    /// Hint-mode alphabet.  Cloned into `HintSession` on each `EnterHintMode`.
    hint_alphabet: HintAlphabet,
    /// One-slot mailbox for `__buffr_hint__:` messages from the renderer.
    ///
    /// Written by the `WebMessageReceived` handler on the STA thread;
    /// drained from any thread by `BrowserEngine::pump_hint_events`.
    hint_sink: HintEventSink,
    /// Active hint session for the current tab, if any.
    ///
    /// Stored behind an `Arc<Mutex<…>>` so `pump_hint_events` (called from any
    /// thread) can mutate it without touching the STA thread.
    hint_session: Arc<Mutex<Option<HintSession>>>,
    /// Shared popup URL queue.  Filled by the STA thread's `NewWindowRequested`
    /// event handler (one URL per `window.open` call); cloned out by
    /// `BrowserEngine::popup_queue()` so the apps layer can drain it each tick.
    popup_queue: PopupQueue,
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

        // Clone Arc<AtomicBool>s and shared queues from inside EngineState so
        // the engine can read / drain them from any thread without locking the
        // Mutex on every call.
        let (
            audio_active,
            video_active,
            loading_active,
            favicon_updates,
            popup_queue,
            cursor_change,
        ) = engine_state
            .lock()
            .map(|g| {
                (
                    Arc::clone(&g.audio_active),
                    Arc::clone(&g.video_active),
                    Arc::clone(&g.loading_active),
                    Arc::clone(&g.favicon_updates),
                    Arc::clone(&g.popup_queue),
                    Arc::clone(&g.cursor_change),
                )
            })
            .unwrap_or_else(|_| {
                use buffr_engine::popup::new_popup_queue;
                (
                    Arc::new(AtomicBool::new(false)),
                    Arc::new(AtomicBool::new(false)),
                    Arc::new(AtomicBool::new(false)),
                    Arc::new(Mutex::new(Vec::new())),
                    new_popup_queue(),
                    Arc::new(Mutex::new(None)),
                )
            });

        let clipboard = hjkl_clipboard::Clipboard::new()
            .map(std::sync::Arc::new)
            .map_err(|e| tracing::debug!(error = %e, "webview2: clipboard init failed"))
            .ok();

        let data_dir = options.data_dir.map(|p| p.to_path_buf());

        // Downcast apps-layer sinks from type-erased Arc<dyn Any>.
        // Each is optional — `None` means the caller didn't wire it (e.g.
        // private mode skips history; stub backends pass None for all).
        let history: Option<Arc<buffr_history::History>> = options
            .history
            .as_ref()
            .and_then(|a| a.clone().downcast::<buffr_history::History>().ok());
        let downloads: Option<Arc<buffr_downloads::Downloads>> = options
            .downloads
            .as_ref()
            .and_then(|a| a.clone().downcast::<buffr_downloads::Downloads>().ok());
        let notice_queue: Option<buffr_core::download_notice::DownloadNoticeQueue> = options
            .notice_queue
            .as_ref()
            .and_then(|a| {
                a.clone()
                    .downcast::<std::sync::Mutex<std::collections::VecDeque<buffr_core::DownloadNotice>>>()
                    .ok()
            });
        let find_sink: Option<FindResultSink> = options.find_sink.as_ref().and_then(|a| {
            a.clone()
                .downcast::<std::sync::Mutex<Option<buffr_core::find::FindResult>>>()
                .ok()
        });

        // Hint-mode infrastructure: alphabet, event sink, session slot.
        // The hint_alphabet falls back to the buffr-core default if the
        // options struct doesn't carry one yet.
        let hint_alphabet = HintAlphabet::from_str(buffr_core::hint::DEFAULT_HINT_ALPHABET)
            .expect("default hint alphabet is always valid");
        let hint_sink = new_hint_event_sink();
        let hint_session: Arc<Mutex<Option<HintSession>>> = Arc::new(Mutex::new(None));

        let worker = spawn(
            options.initial_url,
            width,
            height,
            Arc::clone(&frame),
            Arc::clone(&view),
            Arc::clone(&engine_state),
            Arc::clone(&hint_sink),
            data_dir,
            history,
            downloads,
            notice_queue,
            find_sink,
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
            video_active,
            cursor_change,
            clipboard,
            loading_active,
            find_query: Mutex::new(None),
            favicon_updates,
            hint_alphabet,
            hint_sink,
            hint_session,
            popup_queue,
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

    fn open_tab_at(&self, url: &str, insert_idx: usize) -> Result<TabId, EngineError> {
        tracing::debug!("webview2: open_tab_at {url} insert_idx={insert_idx}");
        self.worker
            .call(|reply| Command::OpenTabAt {
                url: url.to_owned(),
                insert_idx,
                reply,
            })
            .map_err(EngineError::from)?
            .map_err(EngineError::from)
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

    fn move_tab(&self, from: usize, to: usize) {
        tracing::debug!("webview2: move_tab {from} → {to}");
        self.worker.send(Command::MoveTab { from, to });
    }

    fn duplicate_active(&self) -> Result<TabId, EngineError> {
        tracing::debug!("webview2: duplicate_active");
        let url = self.active_tab().map(|t| t.url).unwrap_or_default();
        let target = if url.is_empty() {
            "about:blank".to_string()
        } else {
            url
        };
        self.open_tab(&target)
    }

    fn toggle_pin_active(&self) {}
    fn set_pinned(&self, _id: TabId, _pinned: bool) {}

    fn reopen_closed_tab(&self) -> Result<Option<TabId>, EngineError> {
        tracing::debug!("webview2: reopen_closed_tab");
        self.worker
            .call(|reply| Command::ReopenClosed { reply })
            .map_err(EngineError::from)?
            .map_err(EngineError::from)
    }

    fn closed_stack_len(&self) -> usize {
        self.with_state(|st| st.closed_stack.len())
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

    // ── Loading state ────────────────────────────────────────────────────────

    /// Read the active tab's loading flag from an `Arc<AtomicBool>` kept in
    /// sync by `NavigationStarting` / `NavigationCompleted` event handlers
    /// and `sync_active_idx` on tab switches.
    ///
    /// Lock-free; safe to call on any thread.
    fn is_loading(&self) -> bool {
        self.loading_active.load(Ordering::Relaxed)
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
        tracing::debug!("webview2: osr_sleep({sleep})");
        self.worker.send(Command::SetSleep { sleep });
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

    fn start_find(&self, query: &str, forward: bool) {
        if let Ok(mut g) = self.find_query.lock() {
            *g = Some(query.to_owned());
        }
        tracing::debug!(
            query,
            forward,
            "webview2: start_find — dispatching to STA worker"
        );
        self.worker.send(Command::StartFind {
            query: query.to_owned(),
            forward,
        });
    }

    fn stop_find(&self) {
        if let Ok(mut g) = self.find_query.lock() {
            *g = None;
        }
        tracing::debug!("webview2: stop_find");
        self.worker.send(Command::StopFind);
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

    // ── Media / image (Phase 6c, #95) ────────────────────────────────────────

    fn media_play_pause(&self, x: i32, y: i32) {
        let _ = self.run_js(&buffr_engine::media_js::play_pause(x, y));
    }

    fn media_toggle_mute(&self, x: i32, y: i32) {
        let _ = self.run_js(&buffr_engine::media_js::toggle_mute(x, y));
    }

    fn media_toggle_loop(&self, x: i32, y: i32) {
        let _ = self.run_js(&buffr_engine::media_js::toggle_loop(x, y));
    }

    fn media_toggle_controls(&self, x: i32, y: i32) {
        let _ = self.run_js(&buffr_engine::media_js::toggle_controls(x, y));
    }

    fn media_picture_in_picture(&self, x: i32, y: i32) {
        let _ = self.run_js(&buffr_engine::media_js::picture_in_picture(x, y));
    }

    fn image_rotate(&self, x: i32, y: i32, delta_deg: i32) {
        let _ = self.run_js(&buffr_engine::media_js::image_rotate(x, y, delta_deg));
    }

    fn copy_image_url_to_clipboard(&self, url: &str) {
        let _ = self.run_js(&buffr_engine::media_js::copy_image_url(url));
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
        self.video_active.load(Ordering::Relaxed)
    }

    fn run_media_probe(&self) {
        // Evaluate JS that checks for active media elements and emits
        // `__buffr_media__:true/false` via window.chrome.webview.postMessage.
        // The WebMessageReceived handler on the STA thread scrapes the sentinel
        // and flips `video_active`.
        let js = "(() => { \
            const active = Array.from(document.querySelectorAll('video,audio'))\
              .some(el => !el.paused && !el.ended && el.currentTime > 0); \
            window.chrome.webview.postMessage('__buffr_media__:' + active); \
        })();";
        let _ = self.run_js(js);
    }

    // ── Popup ─────────────────────────────────────────────────────────────────
    //
    // WebView2 routes `window.open` through the `NewWindowRequested` event
    // (wired per-tab in `runtime.rs`).  Intercepted URLs are pushed onto the
    // shared `popup_queue`; the apps layer drains it each tick and calls
    // `open_tab`.  OSR-based popup frames are not used — WebView2 handles popup
    // creation as a new-tab re-route; `popup_create_sink` / `popup_close_sink`
    // return fresh empty sinks.

    fn popup_queue(&self) -> PopupQueue {
        Arc::clone(&self.popup_queue)
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

    // ── Hint mode ─────────────────────────────────────────────────────────────

    fn is_hint_mode(&self) -> bool {
        self.hint_session
            .lock()
            .ok()
            .map(|g| g.is_some())
            .unwrap_or(false)
    }

    fn hint_status(&self) -> Option<HintStatus> {
        let guard = self.hint_session.lock().ok()?;
        let s = guard.as_ref()?;
        Some(HintStatus {
            typed: s.typed.clone(),
            match_count: s.match_count(),
            background: s.background,
        })
    }

    fn pump_hint_events(&self) -> bool {
        let Some(event) = take_hint_event(&self.hint_sink) else {
            return false;
        };
        match event {
            buffr_core::hint::HintConsoleEvent::Ready { hints, alphabet: _ } => {
                let alphabet = self.hint_alphabet.clone();
                if let Ok(mut guard) = self.hint_session.lock()
                    && let Some(existing) = guard.as_mut()
                {
                    let background = existing.background;
                    *existing = HintSession::new(alphabet, hints, background);
                }
                true
            }
            buffr_core::hint::HintConsoleEvent::Error { message } => {
                tracing::warn!(message, "webview2: hint mode: renderer reported error");
                self.cancel_hint();
                true
            }
        }
    }

    fn feed_hint_key(&self, c: char) -> Option<HintAction> {
        let mut commit_id: Option<u32> = None;
        let mut filter_typed: Option<String> = None;
        let mut clear = false;
        let mut cancel = false;
        let action = {
            let mut guard = self.hint_session.lock().ok()?;
            let session = guard.as_mut()?;
            let action = session.feed(c);
            let typed = session.typed.clone();
            match &action {
                HintAction::Filter => filter_typed = Some(typed),
                HintAction::Click(id) | HintAction::OpenInBackground(id) => {
                    commit_id = Some(*id);
                    clear = true;
                }
                HintAction::Cancel => cancel = true,
            }
            action
        };
        if let Some(typed) = filter_typed {
            let _ = self.run_js(&format!(
                "if (window.__buffrHintFilter) window.__buffrHintFilter({})",
                js_string_literal(&typed)
            ));
        }
        if let Some(id) = commit_id {
            let _ = self.run_js(&format!(
                "if (window.__buffrHintCommit) window.__buffrHintCommit({id})"
            ));
        }
        if clear && let Ok(mut guard) = self.hint_session.lock() {
            *guard = None;
        }
        if cancel {
            self.cancel_hint();
        }
        Some(action)
    }

    fn backspace_hint(&self) -> Option<HintAction> {
        let mut filter_typed: Option<String> = None;
        let mut cancel = false;
        let action = {
            let mut guard = self.hint_session.lock().ok()?;
            let session = guard.as_mut()?;
            let action = session.backspace();
            let typed = session.typed.clone();
            match &action {
                HintAction::Filter => filter_typed = Some(typed),
                HintAction::Cancel => cancel = true,
                _ => {}
            }
            action
        };
        if let Some(typed) = filter_typed {
            let _ = self.run_js(&format!(
                "if (window.__buffrHintFilter) window.__buffrHintFilter({})",
                js_string_literal(&typed)
            ));
        }
        if cancel {
            self.cancel_hint();
        }
        Some(action)
    }

    fn cancel_hint(&self) {
        tracing::debug!("webview2: cancel_hint");
        let _ = self.run_js("if (window.__buffrHintCancel) window.__buffrHintCancel()");
        if let Ok(mut guard) = self.hint_session.lock() {
            *guard = None;
        }
    }

    // ── Favicon (Gap 1) ───────────────────────────────────────────────────────
    //
    // The STA thread's `FaviconChanged` handler reads `FaviconUri` from
    // `ICoreWebView2_15` and pushes a sentinel `FaviconUpdate` (empty pixels,
    // browser_id = tab_id.0) into the shared queue.  The apps layer drains it
    // each tick and uses the `browser_id` to invalidate its favicon cache.

    fn drain_favicon_updates(&self) -> Vec<FaviconUpdate> {
        match self.favicon_updates.lock() {
            Ok(mut guard) => std::mem::take(&mut *guard),
            Err(_) => Vec::new(),
        }
    }

    // ── Cursor (Gap 2) ────────────────────────────────────────────────────────
    //
    // JS mousemove listener (injected via AddScriptToExecuteOnDocumentCreated
    // in runtime.rs) emits `__buffr_cursor__:<css-cursor>` via
    // window.chrome.webview.postMessage.  The WebMessageReceived handler
    // writes the translated raw kind into `cursor_change`.  We drain the slot
    // here.

    fn take_cursor_change(&self) -> Option<(i32, u32)> {
        self.cursor_change.lock().ok().and_then(|mut g| g.take())
    }

    // ── Clipboard (Gap 4) ─────────────────────────────────────────────────────
    //
    // Primary path: `hjkl_clipboard::Clipboard` — synchronous, any-thread,
    // matches the pattern used by buffr-firefox-cdp and buffr-webkitgtk.
    // Fallback path: STA worker Win32 API (ClipboardRead / ClipboardWrite
    // commands) — used when hjkl-clipboard fails to initialise (e.g. no
    // display server on Windows CI).

    fn clipboard_handle(&self) -> Option<buffr_engine::ClipboardReader> {
        let cb = self.clipboard.clone()?;
        let reader = Wv2ClipboardReader(cb);
        Some(std::sync::Arc::new(reader))
    }

    fn clipboard_text(&self) -> Option<String> {
        use hjkl_clipboard::{MimeType, Selection};
        if let Some(cb) = self.clipboard.as_ref() {
            tracing::debug!("webview2: clipboard_text (hjkl path)");
            return match cb.get(Selection::Clipboard, MimeType::Text) {
                Ok(bytes) => match String::from_utf8(bytes) {
                    Ok(s) if !s.is_empty() => Some(s),
                    Ok(_) => None,
                    Err(err) => {
                        tracing::debug!(error = %err, "webview2: clipboard_text non-utf8");
                        None
                    }
                },
                Err(err) => {
                    tracing::debug!(error = %err, "webview2: clipboard_text read failed");
                    None
                }
            };
        }
        // Fallback: STA worker Win32 path.
        tracing::debug!("webview2: clipboard_text (Win32 worker fallback)");
        self.worker
            .call(|reply| Command::ClipboardRead { reply })
            .unwrap_or(None)
    }

    fn clipboard_set_text(&self, text: &str) -> bool {
        use hjkl_clipboard::{MimeType, Selection};
        if let Some(cb) = self.clipboard.as_ref() {
            tracing::debug!(
                "webview2: clipboard_set_text ({} chars, hjkl path)",
                text.len()
            );
            return match cb.set(Selection::Clipboard, MimeType::Text, text.as_bytes()) {
                Ok(()) => true,
                Err(err) => {
                    tracing::warn!(error = %err, "webview2: clipboard_set_text hjkl failed, falling back to Win32");
                    // Fall through to worker path below.
                    false
                }
            };
        }
        // Fallback: STA worker Win32 path (fire-and-forget, optimistically true).
        tracing::debug!(
            "webview2: clipboard_set_text ({} chars, Win32 worker fallback)",
            text.len()
        );
        self.worker.send(Command::ClipboardWrite {
            text: text.to_owned(),
        });
        true
    }

    // ── Edit IPC (Gap 5) ──────────────────────────────────────────────────────
    //
    // JS injection via `self.run_js`.  The edit.js layer exposes sentinel
    // globals:
    //   window.__buffrEditAttach({field_id})
    //   window.__buffrEditCycle({forward: true/false})
    //   window.__buffrEditDetach({field_id})
    //   window.__buffrEditFocus({field_id})
    //
    // If the page has not loaded edit.js the call is silently discarded by JS.

    fn run_edit_attach(&self, field_id: &str) {
        tracing::debug!("webview2: run_edit_attach {field_id:?}");
        let field_id_json = serde_json::to_string(field_id).unwrap_or_else(|_| "\"\"".into());
        let js = format!(
            "if(typeof window.__buffrEditAttach==='function')window.__buffrEditAttach({{field_id:{field_id_json}}})"
        );
        let _ = self.run_js(&js);
    }

    fn run_edit_cycle(&self, forward: bool) {
        tracing::debug!("webview2: run_edit_cycle forward={forward}");
        let js = format!(
            "if(typeof window.__buffrEditCycle==='function')window.__buffrEditCycle({{forward:{forward}}})"
        );
        let _ = self.run_js(&js);
    }

    fn run_edit_detach(&self, field_id: &str) {
        tracing::debug!("webview2: run_edit_detach {field_id:?}");
        let field_id_json = serde_json::to_string(field_id).unwrap_or_else(|_| "\"\"".into());
        let js = format!(
            "if(typeof window.__buffrEditDetach==='function')window.__buffrEditDetach({{field_id:{field_id_json}}})"
        );
        let _ = self.run_js(&js);
    }

    fn run_edit_focus(&self, field_id: &str) {
        tracing::debug!("webview2: run_edit_focus {field_id:?}");
        let field_id_json = serde_json::to_string(field_id).unwrap_or_else(|_| "\"\"".into());
        let js = format!(
            "if(typeof window.__buffrEditFocus==='function')window.__buffrEditFocus({{field_id:{field_id_json}}})"
        );
        let _ = self.run_js(&js);
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
            //
            // FindNext / FindPrev are handled by re-issuing StartFind so the
            // STA worker calls ICoreWebView2Find::FindNext / FindPrevious on
            // the live session.  The query is retrieved from the cached value
            // stored by `start_find`.
            A::FindNext => {
                let query = self.find_query.lock().ok().and_then(|g| g.clone());
                if let Some(q) = query {
                    tracing::debug!(query = %q, "webview2: dispatch FindNext");
                    self.worker.send(Command::StartFind {
                        query: q,
                        forward: true,
                    });
                } else {
                    tracing::debug!("webview2: FindNext — no active find query");
                }
            }
            A::FindPrev => {
                let query = self.find_query.lock().ok().and_then(|g| g.clone());
                if let Some(q) = query {
                    tracing::debug!(query = %q, "webview2: dispatch FindPrev");
                    self.worker.send(Command::StartFind {
                        query: q,
                        forward: false,
                    });
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

            // ── Hint mode ─────────────────────────────────────────────────────
            A::EnterHintMode => self.enter_hint_mode(false),
            A::EnterHintModeBackground => self.enter_hint_mode(true),

            other => {
                tracing::debug!(
                    action = ?other,
                    "webview2: dispatch — action not handled by this backend (no-op)"
                );
            }
        }
    }
}

// ── Private helpers ───────────────────────────────────────────────────────────

impl WebView2Engine {
    /// Inject `hint.js` into the active tab and open a hint session.
    fn enter_hint_mode(&self, background: bool) {
        const LABEL_BUDGET: usize = 256;
        let labels = self.hint_alphabet.labels_for(LABEL_BUDGET);
        let alphabet_str = self.hint_alphabet.as_string();
        let script = build_inject_script(&alphabet_str, &labels, DEFAULT_HINT_SELECTORS);

        // Seed an empty session immediately so `is_hint_mode` returns `true`
        // before the renderer responds.  `pump_hint_events` replaces it with the
        // real hint list when the `ready` message arrives.
        if let Ok(mut guard) = self.hint_session.lock() {
            *guard = Some(HintSession::new(
                self.hint_alphabet.clone(),
                Vec::new(),
                background,
            ));
        }
        let _ = self.run_js(&script);
        tracing::info!(
            background,
            label_budget = LABEL_BUDGET,
            "webview2: hint mode: injected"
        );
    }
}

// ── hjkl-clipboard → ClipboardRead bridge ────────────────────────────────────

/// Wraps `Arc<hjkl_clipboard::Clipboard>` to implement the engine-agnostic
/// `ClipboardRead` trait.  Mirrors the pattern used by `buffr-firefox-cdp` and
/// `buffr-webkitgtk`.
struct Wv2ClipboardReader(std::sync::Arc<hjkl_clipboard::Clipboard>);

impl buffr_engine::ClipboardRead for Wv2ClipboardReader {
    fn read_text(&self) -> Option<String> {
        use hjkl_clipboard::{MimeType, Selection};
        match self.0.get(Selection::Clipboard, MimeType::Text) {
            Ok(bytes) => String::from_utf8(bytes).ok().filter(|s| !s.is_empty()),
            Err(err) => {
                tracing::debug!(error = %err, "webview2: Wv2ClipboardReader::read_text failed");
                None
            }
        }
    }
}

// ── JS string helpers ─────────────────────────────────────────────────────────

/// Format a Rust string as a JS double-quoted literal with `\uXXXX` escaping
/// for every non-ASCII codepoint.  Mirrors `json_string_literal` from the CEF
/// host so the inline `__buffrHintFilter` / `__buffrHintCancel` calls are safe
/// to splice regardless of what the user typed.
fn js_string_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_ascii_graphic() || c == ' ' => out.push(c),
            c => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf).iter() {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
        }
    }
    out.push('"');
    out
}
