//! [`LadybirdEngine`] — real [`BrowserEngine`] impl backed by the cxx FFI shim.
//!
//! # Phase B status
//!
//! ## Architecture
//!
//! `cxx::UniquePtr<WebContent>` is `Send` but is owned on a dedicated worker
//! thread to mirror what Phase C will require (Ladybird's WebContentServer is
//! a separate process with thread-affine IPC).  `LadybirdEngine` only holds
//! thread-safe handles and communicates via `mpsc` channels.
//!
//! ```text
//! LadybirdEngine (any thread — Send + Sync)
//!   cmd_tx ──── Command ────▶  state::Worker::run()
//!                                ├─ creates / navigates WebContent instances (FFI)
//!                                ├─ calls webcontent_read_pixels after each mutation
//!                                └─ writes BGRA into SharedOsrFrame
//! ```
//!
//! ## Fully implemented (real FFI calls, not stubs)
//!
//! - `close_all_browsers` — drops all `LadybirdTab` instances via CloseTab
//! - `open_tab` / `open_tab_background` / `open_tab_at`
//! - `close_tab` / `close_active`
//! - `select_tab` / `next_tab` / `prev_tab`
//! - `navigate` / `go_back` / `go_forward` / `reload` / `stop`
//! - `active_tab` / `tabs_summary` / `tab_count` / `active_index`
//! - `active_tab_live_url` / `can_go_back` / `can_go_forward`
//! - `osr_resize` — resizes all tabs, writes fresh OSR frame
//! - `osr_frame` / `osr_view` — stable cloned `Arc` handles
//! - `osr_key_event` / `osr_mouse_move` / `osr_mouse_click` / `osr_mouse_leave` / `osr_mouse_wheel`
//! - `force_repaint_active` / `osr_invalidate_view` / `set_osr_wake`
//!
//! ## OSR pipeline (Phase B)
//!
//! `webcontent_read_pixels` fills the buffer with solid dark-grey (BGRA 0xFF101010).
//! Phase C replaces with real LibGfx bitmap readback once Ladybird IPC is wired.
//!
//! ## Still stubbed
//!
//! Popup, hint, find, zoom, devtools, scheme, edit, clipboard, audio, media.

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
use std::sync::atomic::Ordering;

use crate::error::LadybirdError;
use crate::input::{
    LadybirdMouseKind, neutral_click_to_ladybird, neutral_key_to_ladybird,
    neutral_leave_to_ladybird, neutral_move_to_ladybird, neutral_scroll_to_ladybird,
};
use crate::state::{Command, TabCache, WorkerHandle, spawn};

// ── LadybirdEngine ────────────────────────────────────────────────────────────

/// Ladybird browser engine.
///
/// Implements [`BrowserEngine`] (`Send + Sync`) by delegating all WebContent
/// operations to a worker thread (which owns the cxx `UniquePtr<WebContent>`s).
pub struct LadybirdEngine {
    #[allow(dead_code)]
    engine_id: EngineId,
    /// Channel handle to the worker thread.
    worker: WorkerHandle,
    /// Shared OSR frame — worker writes BGRA pixels here after each paint.
    frame: SharedOsrFrame,
    /// Shared viewport state.
    view: SharedOsrViewState,
    /// Cached tab snapshot (Mutex for &self access).
    cache: Mutex<TabCache>,
}

impl LadybirdEngine {
    /// Construct the engine, spawn the worker thread, open the initial URL.
    pub fn new(options: &BackendOpenOptions<'_>) -> Result<Self, LadybirdError> {
        let (width, height) = options.initial_size;

        let frame: SharedOsrFrame = Arc::new(Mutex::new(OsrFrame::new(width, height)));
        let view: SharedOsrViewState = {
            let v = OsrViewState::new();
            v.width.store(width, Ordering::Relaxed);
            v.height.store(height, Ordering::Relaxed);
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
            "ladybird engine created (engine_id={}, url={}, size={width}×{height})",
            options.engine_id.as_str(),
            options.initial_url,
        );

        Ok(LadybirdEngine {
            engine_id: options.engine_id.clone(),
            worker,
            frame,
            view,
            cache: Mutex::new(TabCache::default()),
        })
    }

    /// Fetch a fresh tab snapshot from the worker and update the local cache.
    fn refresh_cache(&self) {
        let snapshot = self
            .worker
            .call(|reply| Command::QueryTabs { reply })
            .unwrap_or_else(|_| crate::state::TabsSnapshot {
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

impl Drop for LadybirdEngine {
    fn drop(&mut self) {
        tracing::info!("ladybird engine: shutting down worker");
        self.worker.send(Command::Shutdown);
    }
}

// ── BrowserEngine impl ────────────────────────────────────────────────────────

impl BrowserEngine for LadybirdEngine {
    // ── Lifecycle ────────────────────────────────────────────────────────────

    fn close_all_browsers(&self) {
        tracing::debug!("ladybird: close_all_browsers");
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
        tracing::debug!("ladybird: open_tab {url}");
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
        tracing::debug!("ladybird: open_tab_at {url} (insert_idx ignored in Phase B)");
        self.open_tab(url)
    }

    fn close_tab(&self, id: TabId) -> Result<bool, EngineError> {
        tracing::debug!("ladybird: close_tab {id:?}");
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
        tracing::debug!("ladybird: select_tab {id:?}");
        self.worker.send(Command::SelectTab { id });
    }

    fn next_tab(&self) {
        self.worker.send(Command::CycleTab { forward: true });
    }

    fn prev_tab(&self) {
        self.worker.send(Command::CycleTab { forward: false });
    }

    fn move_tab(&self, _from: usize, _to: usize) {
        tracing::debug!("ladybird: move_tab not implemented in Phase B");
    }

    fn duplicate_active(&self) -> Result<TabId, EngineError> {
        Err(EngineError::Unimplemented {
            method: "duplicate_active",
        })
    }

    fn toggle_pin_active(&self) {
        tracing::debug!("ladybird: toggle_pin_active not implemented");
    }

    fn set_pinned(&self, _id: TabId, _pinned: bool) {
        tracing::debug!("ladybird: set_pinned not implemented");
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
        tracing::debug!("ladybird: navigate {url}");
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
        true
    }

    // ── Loading state ────────────────────────────────────────────────────────

    /// Ladybird Phase B uses a C++ stub (`webcontent_navigate` is synchronous
    /// at the FFI boundary).  No async load event is wired; always `false`.
    ///
    /// TODO(ladybird-phase-c): add `webcontent_is_loading() -> bool` to the
    /// cxx bridge (`ffi.rs`) and delegate to it here once real Ladybird IPC
    /// (out-of-process WebContentServer) is connected.
    fn is_loading(&self) -> bool {
        false
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
        tracing::debug!("ladybird: set_device_scale {scale}");
        self.view.set_scale(scale);
    }

    fn set_frame_rate(&self, _hz: u32) {
        tracing::debug!("ladybird: set_frame_rate — no-op (event-driven only)");
    }

    fn notify_screen_info_changed(&self) {
        tracing::debug!("ladybird: notify_screen_info_changed — no-op");
    }

    fn osr_resize(&self, width: u32, height: u32) {
        tracing::debug!("ladybird: osr_resize {width}×{height}");
        if let Ok(mut guard) = self.frame.lock() {
            guard.needs_fresh = true;
        }
        self.worker.send(Command::Resize { width, height });
    }

    // ── Input ────────────────────────────────────────────────────────────────

    fn osr_key_event(&self, event: NeutralKeyEvent) {
        let ev = neutral_key_to_ladybird(&event);
        tracing::debug!(
            "ladybird: key_event key={} press={}",
            ev.key_code,
            ev.is_press
        );
        self.worker.send(Command::SendKey {
            key_code: ev.key_code,
            is_press: ev.is_press,
            modifiers: ev.modifiers,
        });
    }

    fn osr_mouse_move(&self, x: i32, y: i32, _modifiers: u32) {
        let ev = neutral_move_to_ladybird(x, y);
        tracing::debug!("ladybird: mouse_move ({}, {})", ev.x, ev.y);
        self.worker
            .send(Command::SendMouseMove { x: ev.x, y: ev.y });
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
        let ev = neutral_click_to_ladybird(x, y, button, mouse_up);
        tracing::debug!("ladybird: mouse_click ({}, {})", ev.x, ev.y);
        if let LadybirdMouseKind::Button { button, is_press } = ev.kind {
            self.worker.send(Command::SendMouseButton {
                button,
                x: ev.x,
                y: ev.y,
                is_press,
            });
        }
    }

    fn osr_mouse_leave(&self, _modifiers: u32) {
        let ev = neutral_leave_to_ladybird();
        tracing::debug!("ladybird: mouse_leave ({}, {})", ev.x, ev.y);
        // Ladybird has no explicit "mouse left the window" IPC; send a move to (0,0).
        self.worker
            .send(Command::SendMouseMove { x: ev.x, y: ev.y });
    }

    fn osr_mouse_wheel(&self, x: i32, y: i32, delta_x: i32, delta_y: i32, _modifiers: u32) {
        let ev = neutral_scroll_to_ladybird(x, y, delta_x, delta_y);
        tracing::debug!("ladybird: mouse_wheel ({}, {})", ev.x, ev.y);
        if let LadybirdMouseKind::Scroll { dx, dy } = ev.kind {
            self.worker.send(Command::SendScroll {
                x: ev.x,
                y: ev.y,
                dx,
                dy,
            });
        }
    }

    fn osr_focus(&self, focused: bool) {
        tracing::debug!("ladybird: osr_focus({focused}) — no-op in Phase B");
    }

    // ── OSR state ────────────────────────────────────────────────────────────

    fn osr_frame(&self) -> SharedOsrFrame {
        Arc::clone(&self.frame)
    }

    fn osr_view(&self) -> SharedOsrViewState {
        Arc::clone(&self.view)
    }

    fn force_repaint_active(&self) {
        tracing::debug!("ladybird: force_repaint_active");
        self.worker.send(Command::ForcePaint);
    }

    fn osr_sleep(&self, sleep: bool) {
        tracing::debug!("ladybird: osr_sleep({sleep}) — no-op");
    }

    fn osr_invalidate_view(&self) {
        tracing::debug!("ladybird: osr_invalidate_view");
        self.worker.send(Command::ForcePaint);
    }

    fn set_osr_wake(&self, wake: Arc<dyn Fn() + Send + Sync>) {
        self.view.set_wake(wake);
        tracing::debug!("ladybird: wake callback installed");
    }

    // ── Find / zoom ──────────────────────────────────────────────────────────

    fn start_find(&self, _query: &str, _forward: bool) {
        tracing::debug!("ladybird: start_find — no-op in Phase B");
    }

    fn stop_find(&self) {
        tracing::debug!("ladybird: stop_find — no-op in Phase B");
    }

    fn active_zoom_level(&self) -> f64 {
        // Query the worker — zoom is owned by the worker thread's LadybirdTab
        // (via the C++ stub's m_zoom field, read back through webcontent_zoom).
        self.worker
            .call(|reply| Command::QueryActiveZoom { reply })
            .unwrap_or(1.0)
    }

    // ── JS execution (Phase 6c, #95) ─────────────────────────────────────────
    //
    // Delegates to the cxx FFI shim via the worker thread.
    //
    // Phase B: the C++ stub logs and no-ops (LibJS is not linked).
    // Phase C: wire to real LibJS dispatch via the Ladybird WebContentServer IPC.
    // TODO(phase-c): replace the no-op shim with real LibJS execution once the
    //   Ladybird embedding IPC surface stabilises.

    fn run_js(&self, code: &str) -> Result<(), EngineError> {
        tracing::debug!("ladybird: run_js ({} bytes)", code.len());
        self.worker.send(Command::EvalJs {
            code: code.to_owned(),
        });
        Ok(())
    }

    fn run_main_frame_js(&self, code: &str, url: &str) -> Result<(), EngineError> {
        tracing::debug!(
            "ladybird: run_main_frame_js ({} bytes, url={url})",
            code.len()
        );
        self.worker.send(Command::EvalMainFrameJs {
            code: code.to_owned(),
            url: url.to_owned(),
        });
        Ok(())
    }

    // ── Downloads (Phase 6c, #95) ────────────────────────────────────────────

    /// Inject a link-click download trigger via `run_js`.
    ///
    /// The JS reaches the C++ stub (`EvalJs` command) and is dropped until
    /// phase C lands (real LibJS dispatch via the Ladybird WebContentServer IPC).
    fn start_download(&self, url: &str) {
        tracing::debug!("ladybird: start_download url={url}");
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

    // TODO(devtools): Ladybird has a separate Inspector process, but our FFI is
    // a stub. Leave trait default until the FFI surface is fleshed out.

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
        tracing::debug!("ladybird: cancel_hint — no-op");
    }

    // ── IME composition (Phase 8d, #86) ──────────────────────────────────────
    //
    // Substrate: `cxx::bridge` FFI functions `webcontent_ime_set_composition`,
    //   `webcontent_ime_commit`, `webcontent_ime_cancel` in
    //   `src/shim/ladybird_shim.{h,cpp}`.
    //
    // Phase B: C++ stub stores preedit in `WebContent::m_ime_composition` (no
    //   render effect). `commit` and `cancel` clear it.
    //
    // Phase C: replace with real Ladybird WebContentServer IPC messages.
    //   Expected message types (TBD at Ladybird API pin time):
    //     Messages::WebContentClient::SetComposition
    //     Messages::WebContentClient::CommitComposition
    //     Messages::WebContentClient::CancelComposition

    fn ime_set_composition(&self, text: &str, cursor: Option<(usize, usize)>) {
        tracing::debug!(text, ?cursor, "ladybird: ime_set_composition");
        let (sel_start, sel_end) = cursor.unwrap_or_else(|| {
            let len = text.len() as u32;
            (len as usize, len as usize)
        });
        self.worker.send(crate::state::Command::ImeSetComposition {
            text: text.to_owned(),
            sel_start: sel_start as u32,
            sel_end: sel_end as u32,
        });
    }

    fn ime_commit(&self, text: &str) {
        tracing::debug!(text, "ladybird: ime_commit");
        self.worker.send(crate::state::Command::ImeCommit {
            text: text.to_owned(),
        });
    }

    fn ime_cancel(&self) {
        tracing::debug!("ladybird: ime_cancel");
        self.worker.send(crate::state::Command::ImeCancel);
    }
}
