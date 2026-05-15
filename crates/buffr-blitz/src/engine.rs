//! [`BlitzEngine`] — real [`BrowserEngine`] impl backed by Blitz DOM/layout.
//!
//! # Phase B status
//!
//! ## Architecture
//!
//! `HtmlDocument` (and Stylo's `BaseDocument`) is `!Send + !Sync` because it
//! contains non-thread-safe Stylo internals.  `BrowserEngine` requires
//! `Send + Sync`.  We use the same pattern as `buffr-servo`: a dedicated
//! worker thread owns all `HtmlDocument` instances; `BlitzEngine` only holds
//! thread-safe handles and communicates via `mpsc` channels.
//!
//! ```text
//! BlitzEngine (any thread — Send + Sync)
//!   cmd_tx ──── Command ────▶  worker::run()
//!                                ├─ opens / navigates real HtmlDocuments
//!                                ├─ runs Stylo + Taffy layout
//!                                └─ paints placeholder BGRA into SharedOsrFrame
//! ```
//!
//! ## Fully implemented (real Blitz types, not stubs)
//!
//! - `close_all_browsers` — drops all `BlitzTab` instances
//! - `open_tab` / `open_tab_background` / `open_tab_at` — create a real
//!   `HtmlDocument` via `blitz-html`, fetch HTML with `ureq`, run Stylo+Taffy
//!   layout, return a stable `TabId`
//! - `close_tab` / `close_active`
//! - `select_tab` / `next_tab` / `prev_tab`
//! - `navigate` — re-fetches HTML, replaces the `HtmlDocument`, re-runs layout
//! - `reload` — re-fetches the current URL
//! - `go_back` / `go_forward` — URL history tracked per-tab in `BlitzTab`
//! - `active_tab` / `tabs_summary` / `tab_count` / `active_index`
//! - `osr_resize` — resizes all tabs, re-runs layout, writes fresh OSR frame
//! - `osr_frame` / `osr_view` — return stable cloned `Arc` handles
//! - `osr_key_event` / `osr_mouse_*` — translated via `input.rs`, logged
//! - `can_go_back` / `can_go_forward` — live from per-tab history stacks
//!
//! ## OSR pipeline (Phase B)
//!
//! `anyrender_vello` requires `wgpu ^28`; the workspace pins `wgpu = "29"`.
//! Cargo cannot unify these, so the GPU readback path is skipped for Phase B.
//! On resize / tab-switch we write a solid white BGRA frame into
//! `SharedOsrFrame`.  Phase C wires the real Vello pipeline once wgpu aligns.
//!
//! ## Still stubbed
//!
//! Popup, hint, find, zoom, devtools, scheme, edit, clipboard, audio, media.

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

use crate::error::BlitzError;
use crate::input::{
    neutral_click_to_blitz, neutral_key_to_blitz, neutral_leave_to_blitz, neutral_move_to_blitz,
    neutral_scroll_to_blitz,
};
use crate::worker::{Command, WorkerHandle, spawn};

// ── BlitzEngine ───────────────────────────────────────────────────────────────

/// Blitz browser engine.
///
/// Implements [`BrowserEngine`] (`Send + Sync`) by delegating all document
/// operations to a worker thread (which owns the `!Send` Blitz documents).
pub struct BlitzEngine {
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

/// Cached snapshot of the worker's tab list.
#[derive(Default)]
struct TabCache {
    summaries: Vec<TabSummary>,
    active_idx: Option<usize>,
}

impl BlitzEngine {
    /// Construct the engine, spawn the worker thread, open the initial URL.
    pub fn new(options: &BackendOpenOptions<'_>) -> Result<Self, BlitzError> {
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
            "blitz engine created (engine_id={}, url={}, size={width}×{height})",
            options.engine_id.as_str(),
            options.initial_url,
        );

        Ok(BlitzEngine {
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

impl Drop for BlitzEngine {
    fn drop(&mut self) {
        tracing::info!("blitz engine: shutting down worker");
        self.worker.send(Command::Shutdown);
    }
}

// ── BrowserEngine impl ────────────────────────────────────────────────────────

impl BrowserEngine for BlitzEngine {
    // ── Lifecycle ────────────────────────────────────────────────────────────

    fn close_all_browsers(&self) {
        tracing::debug!("blitz: close_all_browsers");
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
        tracing::debug!("blitz: open_tab {url}");
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
        tracing::debug!("blitz: open_tab_at {url} (insert_idx ignored in Phase B)");
        self.open_tab(url)
    }

    fn close_tab(&self, id: TabId) -> Result<bool, EngineError> {
        tracing::debug!("blitz: close_tab {id:?}");
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
        tracing::debug!("blitz: select_tab {id:?}");
        self.worker.send(Command::SelectTab { id });
    }

    fn next_tab(&self) {
        self.worker.send(Command::CycleTab { forward: true });
    }

    fn prev_tab(&self) {
        self.worker.send(Command::CycleTab { forward: false });
    }

    fn move_tab(&self, _from: usize, _to: usize) {
        tracing::debug!("blitz: move_tab not implemented in Phase B");
    }

    fn duplicate_active(&self) -> Result<TabId, EngineError> {
        Err(EngineError::Unimplemented {
            method: "duplicate_active",
        })
    }

    fn toggle_pin_active(&self) {
        tracing::debug!("blitz: toggle_pin_active not implemented");
    }

    fn set_pinned(&self, _id: TabId, _pinned: bool) {
        tracing::debug!("blitz: set_pinned not implemented");
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
        tracing::debug!("blitz: navigate {url}");
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

    /// Blitz uses a synchronous `ureq` fetch: by the time `navigate` returns
    /// the document is already parsed.  The active tab is never "in flight"
    /// from the engine thread's perspective, so this always returns `false`.
    fn is_loading(&self) -> bool {
        false
    }

    // ── Viewport ─────────────────────────────────────────────────────────────

    fn resize(&self, width: u32, height: u32) {
        self.osr_resize(width, height);
    }

    fn set_device_scale(&self, scale: f32) {
        tracing::debug!("blitz: set_device_scale {scale}");
        self.view.set_scale(scale);
    }

    fn set_frame_rate(&self, _hz: u32) {
        tracing::debug!("blitz: set_frame_rate — no-op (event-driven only)");
    }

    fn notify_screen_info_changed(&self) {
        tracing::debug!("blitz: notify_screen_info_changed — no-op");
    }

    fn osr_resize(&self, width: u32, height: u32) {
        tracing::debug!("blitz: osr_resize {width}×{height}");
        if let Ok(mut guard) = self.frame.lock() {
            guard.needs_fresh = true;
        }
        self.worker.send(Command::Resize { width, height });
    }

    // ── Input ────────────────────────────────────────────────────────────────

    fn osr_key_event(&self, event: NeutralKeyEvent) {
        let ev = neutral_key_to_blitz(&event);
        tracing::debug!("blitz: osr_key_event — dispatching to worker");
        self.worker.send(Command::SendInput(ev));
    }

    fn osr_mouse_move(&self, x: i32, y: i32, _modifiers: u32) {
        let ev = neutral_move_to_blitz(x, y);
        tracing::debug!("blitz: osr_mouse_move ({x},{y}) — dispatching to worker");
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
        let ev = neutral_click_to_blitz(x, y, button, mouse_up);
        tracing::debug!("blitz: osr_mouse_click ({x},{y}) up={mouse_up} — dispatching to worker");
        self.worker.send(Command::SendInput(ev));
    }

    fn osr_mouse_leave(&self, _modifiers: u32) {
        let ev = neutral_leave_to_blitz();
        tracing::debug!("blitz: osr_mouse_leave — dispatching to worker");
        self.worker.send(Command::SendInput(ev));
    }

    fn osr_mouse_wheel(&self, x: i32, y: i32, delta_x: i32, delta_y: i32, _modifiers: u32) {
        let ev = neutral_scroll_to_blitz(x, y, delta_x, delta_y);
        tracing::debug!("blitz: osr_mouse_wheel ({x},{y}) — dispatching to worker");
        self.worker.send(Command::SendInput(ev));
    }

    fn osr_focus(&self, focused: bool) {
        tracing::debug!("blitz: osr_focus({focused}) — no-op in Phase B");
    }

    // ── OSR state ────────────────────────────────────────────────────────────

    fn osr_frame(&self) -> SharedOsrFrame {
        Arc::clone(&self.frame)
    }

    fn osr_view(&self) -> SharedOsrViewState {
        Arc::clone(&self.view)
    }

    fn force_repaint_active(&self) {
        tracing::debug!("blitz: force_repaint_active");
        self.worker.send(Command::ForcePaint);
    }

    fn osr_sleep(&self, _sleep: bool) {
        // Blitz renders purely on-demand (event-driven, no idle paint loop).
        // osr_sleep is a CEF concept — there is no background loop to pause.
        tracing::debug!("blitz: osr_sleep — no-op (on-demand render, no idle loop)");
    }

    fn osr_invalidate_view(&self) {
        tracing::debug!("blitz: osr_invalidate_view");
        self.worker.send(Command::ForcePaint);
    }

    fn set_osr_wake(&self, wake: Arc<dyn Fn() + Send + Sync>) {
        self.view.set_wake(wake);
        tracing::debug!("blitz: wake callback installed");
    }

    // ── Find / zoom ──────────────────────────────────────────────────────────

    fn start_find(&self, _query: &str, _forward: bool) {
        tracing::debug!("blitz: start_find — no-op in Phase B");
    }

    fn stop_find(&self) {
        tracing::debug!("blitz: stop_find — no-op in Phase B");
    }

    fn active_zoom_level(&self) -> f64 {
        // Query the worker — zoom is owned by !Send BlitzTab.
        self.worker
            .call(|reply| Command::QueryActiveZoom { reply })
            .unwrap_or(1.0)
    }

    // ── JS execution (Phase 6c, #95) ─────────────────────────────────────────
    //
    // Blitz is a CSS/layout engine with no JavaScript runtime. Returning
    // `Err(EngineError::Other(...))` gives the apps layer a real signal instead
    // of the default `Err(Unimplemented)`, and clearly documents the capability
    // boundary. Any caller that checks the error can fall back gracefully.

    fn run_js(&self, _code: &str) -> Result<(), EngineError> {
        Err(EngineError::Other("blitz: no JS engine".into()))
    }

    fn run_main_frame_js(&self, _code: &str, _url: &str) -> Result<(), EngineError> {
        Err(EngineError::Other("blitz: no JS engine".into()))
    }

    // ── Downloads (Phase 6c, #95) ────────────────────────────────────────────

    fn start_download(&self, url: &str) {
        tracing::debug!("blitz: start_download url={url}");
        tracing::warn!("blitz: start_download not supported (no JS engine)");
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

    // ── Navigation history capability ─────────────────────────────────────────

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

    // ── Audio / video ────────────────────────────────────────────────────────

    fn any_audio_active(&self) -> bool {
        false
    }

    fn any_video_active(&self) -> bool {
        // Blitz has no media element support — video decoding is not available.
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

    // Hint mode unsupported on blitz (no JS engine).

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

    fn cancel_hint(&self) {}

    // ── Frame editing stubs ──────────────────────────────────────────────────
    //
    // Blitz is a read-only CSS/layout renderer — there is no editable DOM
    // surface and no JavaScript engine to drive edit commands.  Each method
    // below is an explicit STUB so future contributors know these are not
    // accidental omissions.

    fn frame_undo(&self) {
        tracing::warn!("blitz: frame_undo unsupported (read-only render, no JS engine)");
    }

    fn frame_redo(&self) {
        tracing::warn!("blitz: frame_redo unsupported (read-only render, no JS engine)");
    }

    fn frame_cut(&self) {
        tracing::warn!("blitz: frame_cut unsupported (read-only render, no JS engine)");
    }

    fn frame_copy(&self) {
        tracing::warn!("blitz: frame_copy unsupported (read-only render, no JS engine)");
    }

    fn frame_paste(&self) {
        tracing::warn!("blitz: frame_paste unsupported (read-only render, no JS engine)");
    }

    fn frame_paste_plain(&self) {
        tracing::warn!("blitz: frame_paste_plain unsupported (read-only render, no JS engine)");
    }

    fn frame_select_all(&self) {
        tracing::warn!("blitz: frame_select_all unsupported (read-only render, no JS engine)");
    }

    // ── Media stubs ───────────────────────────────────────────────────────────
    //
    // Media ops inject JS into the active frame — no JS engine means these
    // are permanently unsupported on the Blitz backend.

    fn media_play_pause(&self, _x: i32, _y: i32) {
        tracing::warn!("blitz: media_play_pause unsupported (read-only render, no JS engine)");
    }

    fn media_picture_in_picture(&self, _x: i32, _y: i32) {
        tracing::warn!(
            "blitz: media_picture_in_picture unsupported (read-only render, no JS engine)"
        );
    }

    fn media_toggle_controls(&self, _x: i32, _y: i32) {
        tracing::warn!("blitz: media_toggle_controls unsupported (read-only render, no JS engine)");
    }

    fn media_toggle_loop(&self, _x: i32, _y: i32) {
        tracing::warn!("blitz: media_toggle_loop unsupported (read-only render, no JS engine)");
    }

    fn media_toggle_mute(&self, _x: i32, _y: i32) {
        tracing::warn!("blitz: media_toggle_mute unsupported (read-only render, no JS engine)");
    }

    fn image_rotate(&self, _x: i32, _y: i32, _delta_deg: i32) {
        tracing::warn!("blitz: image_rotate unsupported (read-only render, no JS engine)");
    }

    // ── Clipboard stubs ───────────────────────────────────────────────────────

    fn copy_image_url_to_clipboard(&self, _url: &str) {
        tracing::warn!(
            "blitz: copy_image_url_to_clipboard unsupported (no clipboard wiring in Blitz backend)"
        );
    }

    // ── Edit IPC stubs ────────────────────────────────────────────────────────
    //
    // These helpers drive the JS edit layer (`edit.js`) — no JS engine means
    // they are permanently unsupported on the Blitz backend.

    fn run_edit_attach(&self, _field_id: &str) {
        tracing::warn!("blitz: run_edit_attach unsupported (read-only render, no JS engine)");
    }

    fn run_edit_cycle(&self, _forward: bool) {
        tracing::warn!("blitz: run_edit_cycle unsupported (read-only render, no JS engine)");
    }

    fn run_edit_detach(&self, _field_id: &str) {
        tracing::warn!("blitz: run_edit_detach unsupported (read-only render, no JS engine)");
    }

    fn run_edit_focus(&self, _field_id: &str) {
        tracing::warn!("blitz: run_edit_focus unsupported (read-only render, no JS engine)");
    }

    // ── Cursor stub ───────────────────────────────────────────────────────────

    fn take_cursor_change(&self) -> Option<(i32, u32)> {
        // TODO: Blitz does not track hover/cursor state yet. Return None until
        // blitz-traits exposes a cursor-change callback in the render pipeline.
        None
    }

    // ── IME composition (Phase 8d, #86) ──────────────────────────────────────
    //
    // blitz-traits 0.3.0-alpha.2 defines `UiEvent::Ime(BlitzImeEvent)` with:
    //   - `Preedit(String, Option<(usize, usize)>)` — live preedit update
    //   - `Commit(String)` — accepted text insertion
    //   - `Disabled` — composition cancelled / IME disabled
    //   - `Enabled` — IME enabled (informational; no text yet)
    //
    // Source: blitz-traits-0.3.0-alpha.2/src/events.rs:65 (UiEvent::Ime),
    //         lines 651–695 (BlitzImeEvent variants).
    //
    // All three are dispatched through `Command::SendInput(BlitzInputEvent::Ime)`,
    // which is already matched in `run()` via the `BlitzInputEvent::into_ui_event()`
    // conversion that now maps `BlitzInputEvent::Ime(ev) → UiEvent::Ime(ev)`.

    fn ime_set_composition(&self, text: &str, cursor: Option<(usize, usize)>) {
        tracing::debug!(text, ?cursor, "blitz: ime_set_composition");
        let ev = crate::input::ime_preedit_to_blitz(text, cursor);
        self.worker.send(Command::SendInput(ev));
    }

    fn ime_commit(&self, text: &str) {
        tracing::debug!(text, "blitz: ime_commit");
        let ev = crate::input::ime_commit_to_blitz(text);
        self.worker.send(Command::SendInput(ev));
    }

    fn ime_cancel(&self) {
        tracing::debug!("blitz: ime_cancel");
        let ev = crate::input::ime_cancel_to_blitz();
        self.worker.send(Command::SendInput(ev));
    }

    // ── Action dispatch ───────────────────────────────────────────────────────
    //
    // History/reload/stop routed through existing worker Commands.
    // Zoom delegates to the existing zoom_* helpers.
    // Find: Blitz has no JS engine so text-find is unsupported — explicit warn
    // arm so contributors know it's a known gap, not an oversight.
    // Scroll and remaining actions: no JS eval substrate in Phase B, fall
    // through to debug-log.

    fn dispatch(&self, action: &buffr_modal::PageAction) {
        use buffr_modal::PageAction as A;

        match action {
            // ── History / reload / stop ───────────────────────────────────────
            A::HistoryBack => {
                tracing::debug!("blitz: dispatch HistoryBack");
                self.worker.send(Command::GoBack { n: 1 });
            }
            A::HistoryForward => {
                tracing::debug!("blitz: dispatch HistoryForward");
                self.worker.send(Command::GoForward { n: 1 });
            }
            A::Reload | A::ReloadHard => {
                tracing::debug!("blitz: dispatch Reload");
                self.worker.send(Command::Reload);
            }
            A::StopLoading => {
                tracing::debug!("blitz: dispatch StopLoading");
                self.worker.send(Command::Stop);
            }

            // ── Zoom ──────────────────────────────────────────────────────────
            A::ZoomIn => self.zoom_in(),
            A::ZoomOut => self.zoom_out(),
            A::ZoomReset => self.zoom_reset(),

            // ── Find — no JS engine, unsupported ──────────────────────────────
            A::Find { .. } | A::FindNext | A::FindPrev => {
                tracing::warn!("blitz: find unsupported (read-only render, no JS engine)");
            }

            // ── Scroll and all other actions — no JS eval substrate in Phase B
            other => {
                tracing::debug!(
                    action = ?other,
                    "blitz: dispatch — action not yet implemented for this backend (no-op)"
                );
            }
        }
    }
}
