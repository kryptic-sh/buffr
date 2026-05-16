//! [`WebKitGtkEngine`] — real [`BrowserEngine`] impl for the WebKitGTK backend.
//!
//! # Phase B status
//!
//! ## Architecture
//!
//! `webkit6::WebView` is a GTK main-thread-only object. `WebKitGtkEngine` is
//! `Send + Sync` (required by `BrowserEngine`). The engine holds only
//! thread-safe handles and communicates with the GTK thread via `mpsc`:
//!
//! ```text
//! WebKitGtkEngine (any thread — Send + Sync)
//!   cmd_tx ──── Command ────▶  worker::GtkRuntime (GTK main thread)
//!                                ├─ owns webkit6::WebView per tab
//!                                └─ wires WebView signals → EngineState
//!   engine_state ────────────────────────────────▶ Arc<Mutex<EngineState>>
//!                (written by signal handlers on GTK thread; read from any thread)
//! ```
//!
//! ## Fully implemented (real WebKitGTK types)
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
//! - `osr_key_event` / `osr_mouse_move` / `osr_mouse_click` / `osr_mouse_leave`
//! - `osr_mouse_wheel`
//!
//! ## Still stubbed
//!
//! popup, hint, find, zoom, downloads, devtools, scheme, edit, clipboard.
//! Input synthesis (key/mouse events) is logged but not yet dispatched to the
//! WebView — see `input.rs` TODO markers.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use buffr_core::DownloadNoticeQueue;
use buffr_core::find::{FindResultSink, new_sink as new_find_sink};
use buffr_core::hint::{
    DEFAULT_HINT_SELECTORS, HintAlphabet, HintConsoleEvent, HintEventSink, HintSession,
    build_inject_script, new_hint_event_sink, take_hint_event,
};
use buffr_downloads::Downloads;
use buffr_engine::{
    BackendOpenOptions, BrowserEngine, EngineError, HintAction, HintStatus, MouseButton,
    NeutralKeyEvent, OsrFrame, OsrViewState, SharedOsrFrame, SharedOsrViewState, TabId, TabSummary,
    engine_id::EngineId,
    popup::{
        PopupCloseSink, PopupCreateSink, PopupQueue, new_popup_close_sink, new_popup_create_sink,
        new_popup_queue,
    },
};
use buffr_history::History;

use super::error::WebKitGtkError;
use super::input::{
    GtkImeEvent, GtkInputEvent, GtkWheelEvent, neutral_click_to_gtk, neutral_key_to_gtk,
    neutral_leave_to_gtk, neutral_move_to_gtk, neutral_scroll_to_gtk,
};
use super::worker::{Command, EngineState, WorkerHandle, spawn};

// ── WebKitGtkEngine ───────────────────────────────────────────────────────────

/// WebKitGTK browser engine.
///
/// Implements [`BrowserEngine`] (`Send + Sync`) by forwarding all WebView
/// operations to the GTK main thread via a worker channel.
pub struct WebKitGtkEngine {
    #[allow(dead_code)]
    engine_id: EngineId,
    /// Shared OSR frame — written by the 4 fps snapshot tick.
    frame: SharedOsrFrame,
    /// Shared viewport state.
    view: SharedOsrViewState,
    /// Worker channel handle.
    worker: WorkerHandle,
    /// Thread-safe tab snapshot. Updated by WebView signal handlers.
    engine_state: Arc<Mutex<EngineState>>,
    /// Cached "any tab playing audio?" flag.  Written by the 500 ms GTK-thread
    /// poll tick; read from any thread by `any_audio_active()`.
    audio_active: Arc<AtomicBool>,
    /// Cached loading state of the **active** tab.
    ///
    /// Written by the `load-changed` GTK signal (via `EngineState::sync_loading_active`)
    /// and by `sync_active_idx` on tab switches.  Read lock-free from any
    /// thread by `BrowserEngine::is_loading`.
    loading_active: Arc<AtomicBool>,
    /// OSR sleep flag — skips snapshot tick when set.
    ///
    /// Cloned from `EngineState::osr_sleeping` so `osr_sleep()` can write it
    /// without locking the full EngineState mutex.
    osr_sleeping: Arc<AtomicBool>,
    /// Last find query stored by `start_find`; re-used by `dispatch` for
    /// `FindNext` / `FindPrev`. Cleared by `stop_find`.
    find_query: std::sync::Mutex<Option<(String, bool)>>,
    // ── Hint mode ────────────────────────────────────────────────────────────
    /// Alphabet used to mint hint labels.
    hint_alphabet: HintAlphabet,
    /// Active hint session for the current tab.  `None` when not in hint mode.
    hint_session: Mutex<Option<HintSession>>,
    /// One-slot mailbox: the script-message handler writes a parsed
    /// `HintConsoleEvent` each time the renderer emits the sentinel.
    hint_sink: HintEventSink,
    // ── Apps-layer sinks ──────────────────────────────────────────────────────
    /// Shared history store — records visits on `LoadEvent::Finished`.
    #[allow(dead_code)]
    history: Option<Arc<History>>,
    /// Shared downloads store — records download lifecycle events.
    #[allow(dead_code)]
    downloads: Option<Arc<Downloads>>,
    /// Download notification queue — surfaced in the chrome strip.
    #[allow(dead_code)]
    notice_queue: Option<DownloadNoticeQueue>,
    /// Find-result sink — written from FindController signal handlers.
    #[allow(dead_code)]
    find_sink: FindResultSink,
    /// Popup URL queue — URLs pushed by the WebView `create` signal handler
    /// when JS calls `window.open(url)` or a link uses `target=_blank`.
    /// Drained by the apps layer via `popup_queue()` to open new tabs.
    popup_queue: PopupQueue,
}

impl WebKitGtkEngine {
    pub fn new(options: &BackendOpenOptions<'_>) -> Result<Self, WebKitGtkError> {
        let (width, height) = options.initial_size;

        let frame: SharedOsrFrame = Arc::new(Mutex::new(OsrFrame::new(width, height)));
        let view: SharedOsrViewState = {
            let v = OsrViewState::new();
            v.width.store(width, Ordering::Relaxed);
            v.height.store(height, Ordering::Relaxed);
            Arc::new(v)
        };

        let engine_state = Arc::new(Mutex::new(EngineState::new(width, height)));

        // Clone Arc<AtomicBool>s from inside EngineState so the engine can
        // read these flags from any thread without locking the Mutex.
        let (audio_active, loading_active, osr_sleeping) = engine_state
            .lock()
            .map(|g| {
                (
                    Arc::clone(&g.audio_active),
                    Arc::clone(&g.loading_active),
                    Arc::clone(&g.osr_sleeping),
                )
            })
            .unwrap_or_else(|_| {
                (
                    Arc::new(AtomicBool::new(false)),
                    Arc::new(AtomicBool::new(false)),
                    Arc::new(AtomicBool::new(false)),
                )
            });

        // ── Downcast apps-layer sinks from BackendOpenOptions ─────────────────
        let history: Option<Arc<History>> = options
            .history
            .as_ref()
            .and_then(|any| any.downcast_ref::<Arc<History>>())
            .cloned();
        let downloads: Option<Arc<Downloads>> = options
            .downloads
            .as_ref()
            .and_then(|any| any.downcast_ref::<Arc<Downloads>>())
            .cloned();
        let notice_queue: Option<DownloadNoticeQueue> = options
            .notice_queue
            .as_ref()
            .and_then(|any| any.downcast_ref::<DownloadNoticeQueue>())
            .cloned();
        let find_sink: FindResultSink = options
            .find_sink
            .as_ref()
            .and_then(|any| any.downcast_ref::<FindResultSink>())
            .cloned()
            .unwrap_or_else(new_find_sink);

        let hint_sink = new_hint_event_sink();
        let popup_queue = new_popup_queue();

        let worker = spawn(
            options.initial_url,
            width,
            height,
            Arc::clone(&frame),
            Arc::clone(&view),
            Arc::clone(&engine_state),
            Arc::clone(&hint_sink),
            history.clone(),
            downloads.clone(),
            notice_queue.clone(),
            find_sink.clone(),
            Arc::clone(&popup_queue),
        )?;

        let hint_alphabet = HintAlphabet::from_str(buffr_core::hint::DEFAULT_HINT_ALPHABET)
            .unwrap_or_else(|_| HintAlphabet::from_str("asdf").expect("fallback alphabet"));

        tracing::info!(
            "webkitgtk engine created (id={}, url={}, size={width}×{height})",
            options.engine_id.as_str(),
            options.initial_url,
        );

        Ok(WebKitGtkEngine {
            engine_id: options.engine_id.clone(),
            frame,
            view,
            worker,
            engine_state,
            audio_active,
            loading_active,
            osr_sleeping,
            find_query: std::sync::Mutex::new(None),
            hint_alphabet,
            hint_session: Mutex::new(None),
            hint_sink,
            history,
            downloads,
            notice_queue,
            find_sink,
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

impl Drop for WebKitGtkEngine {
    fn drop(&mut self) {
        tracing::info!("webkitgtk engine: shutting down worker");
        self.worker.send(Command::Shutdown);
    }
}

// ── BrowserEngine impl ────────────────────────────────────────────────────────

impl BrowserEngine for WebKitGtkEngine {
    // ── Lifecycle ────────────────────────────────────────────────────────────

    fn close_all_browsers(&self) {
        tracing::debug!("webkitgtk: close_all_browsers");
        let ids: Vec<TabId> = self.with_state(|st| st.tabs.iter().map(|t| t.id).collect());
        for id in ids {
            let _ = self.worker.call(|reply| Command::CloseTab { id, reply });
        }
    }

    // ── Tabs ─────────────────────────────────────────────────────────────────

    fn open_tab(&self, url: &str) -> Result<TabId, EngineError> {
        tracing::debug!("webkitgtk: open_tab {url}");
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
        tracing::debug!("webkitgtk: open_tab_at {url} (insert_idx ignored in Phase B)");
        self.open_tab(url)
    }

    fn close_tab(&self, id: TabId) -> Result<bool, EngineError> {
        tracing::debug!("webkitgtk: close_tab {id:?}");
        self.worker
            .call(|reply| Command::CloseTab { id, reply })
            .map_err(EngineError::from)?
            .map_err(EngineError::from)
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
        tracing::debug!("webkitgtk: select_tab {id:?}");
        self.worker.send(Command::SelectTab { id });
    }

    fn next_tab(&self) {
        self.worker.send(Command::CycleTab { forward: true });
    }

    fn prev_tab(&self) {
        self.worker.send(Command::CycleTab { forward: false });
    }

    fn move_tab(&self, _from: usize, _to: usize) {
        tracing::debug!("webkitgtk: move_tab not implemented in Phase B");
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
        tracing::debug!("webkitgtk: navigate {url}");
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

    // ── Loading state ────────────────────────────────────────────────────────

    /// Read the active tab's loading flag from an `Arc<AtomicBool>` kept in
    /// sync by the `load-changed` GTK signal and `sync_active_idx`.
    ///
    /// Lock-free; safe to call on any thread.
    fn is_loading(&self) -> bool {
        self.loading_active.load(Ordering::Relaxed)
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
        tracing::debug!("webkitgtk: set_device_scale {scale}");
        self.view.set_scale(scale);
    }

    fn set_frame_rate(&self, _hz: u32) {
        tracing::debug!("webkitgtk: set_frame_rate — no-op (event-driven)");
    }

    fn notify_screen_info_changed(&self) {
        tracing::debug!("webkitgtk: notify_screen_info_changed — no-op");
    }

    fn osr_resize(&self, width: u32, height: u32) {
        tracing::debug!("webkitgtk: osr_resize {width}×{height}");
        if let Ok(mut guard) = self.frame.lock() {
            guard.needs_fresh = true;
        }
        self.view.width.store(width, Ordering::Relaxed);
        self.view.height.store(height, Ordering::Relaxed);
        self.worker.send(Command::Resize { width, height });
    }

    // ── Input ────────────────────────────────────────────────────────────────
    //
    // Input synthesis in GTK4/webkit6 0.11 requires building synthetic GDK
    // events — safe Rust constructors are not yet exposed. Phase B logs the
    // translated event; Phase C will dispatch via `WebView::event()`.

    fn osr_key_event(&self, event: NeutralKeyEvent) {
        let ev = neutral_key_to_gtk(&event);
        tracing::debug!("webkitgtk: osr_key_event — routing to GTK worker");
        self.worker.send(Command::SendInput(ev));
    }

    fn osr_mouse_move(&self, x: i32, y: i32, _modifiers: u32) {
        let ev = neutral_move_to_gtk(x, y);
        tracing::debug!("webkitgtk: osr_mouse_move ({x},{y}) — routing to GTK worker");
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
        let ev = neutral_click_to_gtk(x, y, button, mouse_up);
        tracing::debug!(
            "webkitgtk: osr_mouse_click ({x},{y}) up={mouse_up} — routing to GTK worker"
        );
        self.worker.send(Command::SendInput(ev));
    }

    fn osr_mouse_leave(&self, _modifiers: u32) {
        let ev = neutral_leave_to_gtk();
        tracing::debug!("webkitgtk: osr_mouse_leave — routing to GTK worker");
        self.worker.send(Command::SendInput(ev));
    }

    fn osr_mouse_wheel(&self, x: i32, y: i32, delta_x: i32, delta_y: i32, _modifiers: u32) {
        let ev = neutral_scroll_to_gtk(x, y, delta_x, delta_y);
        tracing::debug!("webkitgtk: osr_mouse_wheel ({x},{y}) — routing to GTK worker");
        self.worker.send(Command::SendInput(ev));
    }

    fn osr_focus(&self, focused: bool) {
        tracing::debug!("webkitgtk: osr_focus({focused}) — no-op in Phase B");
    }

    // ── OSR state ────────────────────────────────────────────────────────────

    fn osr_frame(&self) -> SharedOsrFrame {
        Arc::clone(&self.frame)
    }

    fn osr_view(&self) -> SharedOsrViewState {
        Arc::clone(&self.view)
    }

    fn force_repaint_active(&self) {
        tracing::debug!("webkitgtk: force_repaint_active");
        self.worker.send(Command::ForcePaint);
    }

    fn osr_sleep(&self, sleep: bool) {
        tracing::debug!("webkitgtk: osr_sleep({sleep})");
        // Write the flag lock-free so the snapshot tick sees it immediately.
        self.osr_sleeping.store(sleep, Ordering::Relaxed);
        // Forward to GTK thread so it can mute/unmute the active WebView.
        self.worker.send(Command::SetOsrSleep { sleep });
    }

    fn osr_invalidate_view(&self) {
        tracing::debug!("webkitgtk: osr_invalidate_view");
        self.worker.send(Command::ForcePaint);
    }

    fn set_osr_wake(&self, wake: Arc<dyn Fn() + Send + Sync>) {
        self.view.set_wake(wake);
        tracing::debug!("webkitgtk: wake callback installed");
    }

    // ── Find / zoom ──────────────────────────────────────────────────────────

    fn start_find(&self, query: &str, forward: bool) {
        if let Ok(mut g) = self.find_query.lock() {
            *g = Some((query.to_owned(), forward));
        }
        self.worker.send(Command::StartFind {
            query: query.to_owned(),
            forward,
        });
    }

    fn stop_find(&self) {
        if let Ok(mut g) = self.find_query.lock() {
            *g = None;
        }
        self.worker.send(Command::StopFind);
    }

    fn active_zoom_level(&self) -> f64 {
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
    // Fire-and-forget JS evaluation via GTK worker → WebViewExt::evaluate_javascript.
    // The `url` origin parameter is not accepted by the WebKitGTK API at the
    // level we call it; the document origin is derived from the loaded page.
    //
    // TODO(phase-c): pass a content-world / frame parameter when WebKitGTK
    //   exposes it in a stable Rust binding.

    fn run_js(&self, code: &str) -> Result<(), EngineError> {
        tracing::debug!("webkitgtk: run_js ({} bytes)", code.len());
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
        tracing::debug!("webkitgtk: start_download url={url}");
        // Use WebKitGTK's native download manager (WebViewExt::download_uri).
        // This is more robust than the JS <a> click trick — it handles
        // cross-origin assets, `Content-Disposition: attachment` headers, and
        // direct resource URLs correctly without page-context JS execution.
        self.worker.send(Command::DownloadUri {
            url: url.to_owned(),
        });
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
        tracing::debug!("webkitgtk: open_devtools {tab:?}");
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

    // ── Popup ────────────────────────────────────────────────────────────────

    /// Clone the shared popup URL queue.
    ///
    /// URLs are enqueued by the `WebView::create` signal handler when JS calls
    /// `window.open(url)` or a link uses `target=_blank`. The apps layer drains
    /// this queue and opens each URL as a new tab.
    fn popup_queue(&self) -> PopupQueue {
        Arc::clone(&self.popup_queue)
    }

    /// Return an empty popup-create sink.
    ///
    /// WebKitGTK handles popup windows via the `create` signal (see
    /// `popup_queue`). There are no OSR popup browsers and therefore no
    /// `PopupCreated` events to surface. An empty sink is returned so the
    /// apps layer does not need a special-case.
    fn popup_create_sink(&self) -> PopupCreateSink {
        new_popup_create_sink()
    }

    /// Return an empty popup-close sink.
    ///
    /// WebKitGTK has no OSR popup browser concept; close events are not
    /// generated. An empty sink is returned so the apps layer can drain it
    /// safely without a special-case.
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
            .map(|g| g.is_some())
            .unwrap_or(false)
    }

    fn hint_status(&self) -> Option<HintStatus> {
        let g = self.hint_session.lock().ok()?;
        let s = g.as_ref()?;
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
            HintConsoleEvent::Ready { hints, alphabet: _ } => {
                let alphabet = self.hint_alphabet.clone();
                if let Ok(mut g) = self.hint_session.lock()
                    && let Some(existing) = g.as_mut()
                {
                    let background = existing.background;
                    *existing = HintSession::new(alphabet, hints, background);
                }
                true
            }
            HintConsoleEvent::Error { message } => {
                tracing::warn!(message, "webkitgtk: hint mode renderer error");
                self.cancel_hint();
                true
            }
        }
    }

    fn feed_hint_key(&self, ch: char) -> Option<HintAction> {
        let mut commit_id: Option<u32> = None;
        let mut filter_typed: Option<String> = None;
        let mut clear = false;
        let mut cancel = false;

        let action = {
            let mut g = self.hint_session.lock().ok()?;
            let session = g.as_mut()?;
            let action = session.feed(ch);
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
            let js = format!(
                "if (window.__buffrHintFilter) window.__buffrHintFilter({})",
                serde_json::to_string(&typed).unwrap_or_else(|_| "\"\"".into())
            );
            let _ = self.run_js(&js);
        }
        if let Some(id) = commit_id {
            let js = format!("if (window.__buffrHintCommit) window.__buffrHintCommit({id})");
            let _ = self.run_js(&js);
        }
        if clear && let Ok(mut g) = self.hint_session.lock() {
            *g = None;
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
            let mut g = self.hint_session.lock().ok()?;
            let session = g.as_mut()?;
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
            let js = format!(
                "if (window.__buffrHintFilter) window.__buffrHintFilter({})",
                serde_json::to_string(&typed).unwrap_or_else(|_| "\"\"".into())
            );
            let _ = self.run_js(&js);
        }
        if cancel {
            self.cancel_hint();
        }
        Some(action)
    }

    fn cancel_hint(&self) {
        let _ = self.run_js("if (window.__buffrHintCancel) window.__buffrHintCancel()");
        if let Ok(mut g) = self.hint_session.lock() {
            *g = None;
        }
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

    // ── Favicon (Gap 1) ───────────────────────────────────────────────────────

    fn drain_favicon_updates(&self) -> Vec<buffr_engine::FaviconUpdate> {
        self.worker
            .call(|reply| Command::DrainFaviconUpdates { reply })
            .unwrap_or_default()
    }

    // ── Cursor (Gap 2) ────────────────────────────────────────────────────────

    fn take_cursor_change(&self) -> Option<(i32, u32)> {
        self.worker
            .call(|reply| Command::TakeCursorChange { reply })
            .unwrap_or(None)
    }

    // ── Clipboard (Gap 4) ─────────────────────────────────────────────────────

    /// Read the current clipboard contents as UTF-8 text.
    ///
    /// GDK4 clipboard reads are async — there is no synchronous API available
    /// on the GTK thread without running a nested main loop. Returning `None`
    /// here matches the trait default. Use `clipboard_handle()` on a worker
    /// thread if a blocking read is required in a future phase.
    fn clipboard_text(&self) -> Option<String> {
        // TODO(phase-d): implement via a Command + GDK4 read_text_async with a
        // local GLib main loop spin to collect the result synchronously off the
        // GTK thread.
        None
    }

    fn clipboard_set_text(&self, text: &str) -> bool {
        tracing::debug!("webkitgtk: clipboard_set_text ({} bytes)", text.len());
        // GDK4 `Clipboard::set_text` is synchronous and fire-and-forget.
        // We forward to the GTK thread (where GDK Display is valid) via the
        // Command channel. No reply needed — the write either succeeds or logs.
        self.worker.send(Command::ClipboardSetText {
            text: text.to_owned(),
        });
        true
    }

    // ── Edit IPC (Gap 5) ──────────────────────────────────────────────────────
    //
    // JS injection pattern — calls `window.__buffrEdit*` helpers injected by
    // buffr's edit.js content script via `run_js`.

    fn run_edit_attach(&self, field_id: &str) {
        let id_json = serde_json::to_string(field_id).unwrap_or_else(|_| "\"\"".into());
        let js = format!("window.__buffrEditAttach && window.__buffrEditAttach({id_json});");
        let _ = self.run_js(&js);
    }

    fn run_edit_cycle(&self, forward: bool) {
        let js = format!(
            "window.__buffrEditCycle && window.__buffrEditCycle({});",
            if forward { "true" } else { "false" }
        );
        let _ = self.run_js(&js);
    }

    fn run_edit_detach(&self, field_id: &str) {
        let id_json = serde_json::to_string(field_id).unwrap_or_else(|_| "\"\"".into());
        let js = format!("window.__buffrEditDetach && window.__buffrEditDetach({id_json});");
        let _ = self.run_js(&js);
    }

    fn run_edit_focus(&self, field_id: &str) {
        let id_json = serde_json::to_string(field_id).unwrap_or_else(|_| "\"\"".into());
        let js = format!("window.__buffrEditFocus && window.__buffrEditFocus({id_json});");
        let _ = self.run_js(&js);
    }

    // ── IME composition (Phase 8d, #86) ──────────────────────────────────────
    //
    // WebKitGTK has no synthetic IME event API in the gdk4 0.11 Rust bindings.
    // We approximate via JS CompositionEvent injection through `evaluate_javascript`,
    // which covers the ~80 % case (JS composition handlers, contenteditable, etc.).
    // Native `<input>` / `<textarea>` insertion for `ime_commit` uses
    // `document.execCommand('insertText')`.
    //
    // Phase D: once gdk4 >= 0.12 exposes `GtkIMContext` Rust constructors,
    // replace the JS path with `WebKitWebView::im_context()` + `filter_keypress`.

    fn ime_set_composition(&self, text: &str, cursor: Option<(usize, usize)>) {
        tracing::debug!(text, ?cursor, "webkitgtk: ime_set_composition");
        self.worker.send(Command::SendInput(GtkInputEvent::Ime(
            GtkImeEvent::Preedit {
                text: text.to_owned(),
                cursor,
            },
        )));
    }

    fn ime_commit(&self, text: &str) {
        tracing::debug!(text, "webkitgtk: ime_commit");
        self.worker.send(Command::SendInput(GtkInputEvent::Ime(
            GtkImeEvent::Commit {
                text: text.to_owned(),
            },
        )));
    }

    fn ime_cancel(&self) {
        tracing::debug!("webkitgtk: ime_cancel");
        self.worker
            .send(Command::SendInput(GtkInputEvent::Ime(GtkImeEvent::Cancel)));
    }

    // ── Action dispatch ───────────────────────────────────────────────────────
    //
    // History/stop → existing worker Commands.
    // Scroll → SendInput(GtkInputEvent::Wheel) which the worker dispatches via
    //   wheel_js → `WheelEvent` DOM event → browser-native scroll.
    // Zoom → existing zoom_* helpers.
    // FindNext/FindPrev → re-send StartFind with the stored query / direction.

    fn dispatch(&self, action: &buffr_modal::PageAction) {
        use buffr_modal::PageAction as A;

        /// Pixels per scroll-unit (matches blink-cdp / CEF backend constant).
        const STEP_PX: f64 = 40.0;

        // Helper: fire a synthetic WheelEvent through the SendInput path.
        let scroll = |dy: f64| {
            self.worker
                .send(Command::SendInput(GtkInputEvent::Wheel(GtkWheelEvent {
                    x: 0.0,
                    y: 0.0,
                    delta_x: 0.0,
                    delta_y: dy,
                })));
        };

        match action {
            // ── Find ─────────────────────────────────────────────────────────
            A::FindNext => {
                let qf = self.find_query.lock().ok().and_then(|g| g.clone());
                if let Some((q, _fwd)) = qf {
                    tracing::debug!(query = %q, "webkitgtk: dispatch FindNext");
                    self.worker.send(Command::StartFind {
                        query: q,
                        forward: true,
                    });
                } else {
                    tracing::debug!("webkitgtk: FindNext — no active find query");
                }
            }
            A::FindPrev => {
                let qf = self.find_query.lock().ok().and_then(|g| g.clone());
                if let Some((q, _fwd)) = qf {
                    tracing::debug!(query = %q, "webkitgtk: dispatch FindPrev");
                    self.worker.send(Command::StartFind {
                        query: q,
                        forward: false,
                    });
                } else {
                    tracing::debug!("webkitgtk: FindPrev — no active find query");
                }
            }

            // ── History / reload / stop ───────────────────────────────────────
            A::HistoryBack => {
                tracing::debug!("webkitgtk: dispatch HistoryBack");
                self.worker.send(Command::GoBack);
            }
            A::HistoryForward => {
                tracing::debug!("webkitgtk: dispatch HistoryForward");
                self.worker.send(Command::GoForward);
            }
            A::Reload | A::ReloadHard => {
                tracing::debug!("webkitgtk: dispatch Reload");
                self.worker.send(Command::Reload);
            }
            A::StopLoading => {
                tracing::debug!("webkitgtk: dispatch StopLoading");
                self.worker.send(Command::Stop);
            }

            // ── Scroll via synthetic WheelEvent ───────────────────────────────
            A::ScrollUp(n) => {
                let dy = -(STEP_PX * (*n as f64));
                tracing::debug!(n, dy, "webkitgtk: dispatch ScrollUp");
                scroll(dy);
            }
            A::ScrollDown(n) => {
                let dy = STEP_PX * (*n as f64);
                tracing::debug!(n, dy, "webkitgtk: dispatch ScrollDown");
                scroll(dy);
            }
            A::ScrollLeft(_) | A::ScrollRight(_) => {
                tracing::debug!(action = ?action, "webkitgtk: dispatch ScrollLeft/Right — no-op (vertical only via wheel)");
            }
            A::ScrollPageDown | A::ScrollFullPageDown => {
                tracing::debug!("webkitgtk: dispatch ScrollPageDown (600px approx)");
                scroll(600.0);
            }
            A::ScrollPageUp | A::ScrollFullPageUp => {
                tracing::debug!("webkitgtk: dispatch ScrollPageUp (600px approx)");
                scroll(-600.0);
            }
            A::ScrollHalfPageDown => {
                tracing::debug!("webkitgtk: dispatch ScrollHalfPageDown (300px approx)");
                scroll(300.0);
            }
            A::ScrollHalfPageUp => {
                tracing::debug!("webkitgtk: dispatch ScrollHalfPageUp (300px approx)");
                scroll(-300.0);
            }
            A::ScrollTop | A::ScrollBottom => {
                tracing::debug!(
                    action = ?action,
                    "webkitgtk: dispatch ScrollTop/Bottom — no-op (no absolute scroll without JS eval)"
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
                    "webkitgtk: dispatch — action not handled by this backend (no-op)"
                );
            }
        }
    }
}

// ── Hint mode helpers ─────────────────────────────────────────────────────────

impl WebKitGtkEngine {
    /// Inject hint.js into the active tab and initialise a `HintSession`.
    fn enter_hint_mode(&self, background: bool) {
        const LABEL_BUDGET: usize = 256;
        let labels = self.hint_alphabet.labels_for(LABEL_BUDGET);
        let alphabet_str = self.hint_alphabet.as_string();
        let script = build_inject_script(&alphabet_str, &labels, DEFAULT_HINT_SELECTORS);

        let alphabet = self.hint_alphabet.clone();
        if let Ok(mut g) = self.hint_session.lock() {
            *g = Some(HintSession::new(alphabet, Vec::new(), background));
        }
        let _ = self.run_js(&script);
        tracing::info!(
            background,
            label_budget = LABEL_BUDGET,
            "webkitgtk: hint mode injected"
        );
    }
}
