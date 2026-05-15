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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use buffr_engine::{
    BackendOpenOptions, BrowserEngine, EngineError, FaviconUpdate, HintAction, HintStatus,
    MouseButton, NeutralKeyEvent, OsrFrame, OsrViewState, SharedOsrFrame, SharedOsrViewState,
    TabId, TabSummary,
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
    /// Cached "any tab playing audio?" flag.  Written by the 500 ms GCD main-
    /// queue poll; read from any thread by `any_audio_active()`.
    #[cfg(target_os = "macos")]
    audio_active: Arc<AtomicBool>,
    /// Cached loading state of the **active** tab.
    ///
    /// Written by `WKNavigationDelegate` callbacks and `snapshot_to_engine_state`
    /// (both on the GCD main queue) via `EngineState::sync_loading_active`.
    /// Read lock-free from any thread by `BrowserEngine::is_loading`.
    #[cfg(target_os = "macos")]
    loading_active: Arc<AtomicBool>,
    /// Pending favicon updates pushed by the navigation delegate.
    /// Drained by `drain_favicon_updates` from any thread.
    #[cfg(target_os = "macos")]
    favicon_updates: Arc<Mutex<Vec<FaviconUpdate>>>,
    /// OSR sleep flag — shared with the worker's audio poll tick.
    /// Set by `osr_sleep`; read by the recurring tick functions.
    #[cfg(target_os = "macos")]
    osr_sleep: Arc<AtomicBool>,
    /// Last find query stored by `start_find`; re-used by `dispatch` for
    /// `FindNext` / `FindPrev`. Cleared by `stop_find`.
    /// Available on all platforms so the struct layout is consistent.
    find_query: Mutex<Option<String>>,
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

            // Clone Arc handles from inside EngineState so the engine can
            // read/write these flags from any thread without locking the Mutex.
            let (audio_active, loading_active, favicon_updates, osr_sleep) = engine_state
                .lock()
                .map(|g| {
                    (
                        Arc::clone(&g.audio_active),
                        Arc::clone(&g.loading_active),
                        Arc::clone(&g.favicon_updates),
                        Arc::clone(&g.osr_sleep),
                    )
                })
                .unwrap_or_else(|_| {
                    (
                        Arc::new(AtomicBool::new(false)),
                        Arc::new(AtomicBool::new(false)),
                        Arc::new(Mutex::new(Vec::new())),
                        Arc::new(AtomicBool::new(false)),
                    )
                });

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
                audio_active,
                loading_active,
                favicon_updates,
                osr_sleep,
                find_query: Mutex::new(None),
            });
        }

        // Non-macOS: only compiled and reachable on non-macOS targets.
        // The `#[cfg(target_os = "macos")]` block above returns early on macOS.
        #[cfg(not(target_os = "macos"))]
        Ok(WebKitCocoaEngine {
            engine_id: options.engine_id.clone(),
            frame,
            view,
            find_query: Mutex::new(None),
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

    // ── Loading state ────────────────────────────────────────────────────────

    /// Read the active tab's loading flag from an `Arc<AtomicBool>` kept in
    /// sync by the `WKNavigationDelegate` callbacks and `snapshot_to_engine_state`.
    ///
    /// Lock-free; safe to call on any thread.
    fn is_loading(&self) -> bool {
        #[cfg(target_os = "macos")]
        return self.loading_active.load(Ordering::Relaxed);

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
        tracing::debug!("webkit-cocoa: osr_sleep({sleep})");
        // Store the flag lock-free.  The audio-activity poll tick holds the
        // same Arc<AtomicBool> (cloned from engine_state during init) and will
        // see the updated value on its next 500 ms wakeup.
        #[cfg(target_os = "macos")]
        self.osr_sleep.store(sleep, Ordering::Relaxed);
        #[cfg(not(target_os = "macos"))]
        let _ = sleep;
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

    fn start_find(&self, query: &str, forward: bool) {
        tracing::debug!("webkit-cocoa: start_find query={query:?} forward={forward}");
        if let Ok(mut g) = self.find_query.lock() {
            *g = Some(query.to_owned());
        }
        #[cfg(target_os = "macos")]
        self.worker.send(Command::StartFind {
            query: query.to_owned(),
            forward,
        });
    }

    fn stop_find(&self) {
        tracing::debug!("webkit-cocoa: stop_find");
        if let Ok(mut g) = self.find_query.lock() {
            *g = None;
        }
        #[cfg(target_os = "macos")]
        self.worker.send(Command::StopFind);
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

    // ── JS execution (Phase 6c, #95) ─────────────────────────────────────────
    //
    // Dispatch a fire-and-forget `WKWebView::evaluateJavaScript:completionHandler:`
    // call to the active tab via the GCD main-queue worker.  The `url` script
    // origin parameter is not accepted by the simple `evaluateJavaScript:` API;
    // WKWebView derives the origin from the currently loaded document.
    //
    // TODO(phase-c): switch to
    //   `WKWebView::evaluateJavaScript:inFrame:inContentWorld:completionHandler:`
    //   to surface evaluation errors via `tracing::warn!`.

    fn run_js(&self, code: &str) -> Result<(), EngineError> {
        tracing::debug!("webkit-cocoa: run_js ({} bytes)", code.len());
        #[cfg(target_os = "macos")]
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

    // ── Clipboard (Gap 6) ────────────────────────────────────────────────────
    //
    // NSPasteboard must be called on the macOS main thread.  `clipboard_text`
    // dispatches a `ClipboardRead` command to the worker which round-trips
    // through the GCD main queue and waits for a reply.
    //
    // `clipboard_set_text` is fire-and-forget: the write command is dispatched
    // and we return immediately without confirmation.
    //
    // Both methods are no-ops on non-macOS targets (stub compile path).

    fn clipboard_text(&self) -> Option<String> {
        #[cfg(target_os = "macos")]
        return self
            .worker
            .call(|reply| Command::ClipboardRead { reply })
            .unwrap_or(None);

        #[cfg(not(target_os = "macos"))]
        None
    }

    fn clipboard_set_text(&self, text: &str) -> bool {
        tracing::debug!("webkit-cocoa: clipboard_set_text ({} bytes)", text.len());
        #[cfg(target_os = "macos")]
        {
            self.worker.send(Command::ClipboardWrite {
                text: text.to_owned(),
            });
            return true;
        }
        #[cfg(not(target_os = "macos"))]
        false
    }

    // ── Downloads (Phase 6c, #95) ────────────────────────────────────────────

    fn start_download(&self, url: &str) {
        tracing::debug!("webkit-cocoa: start_download url={url}");
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

    // ── Favicon (Gap 1) ──────────────────────────────────────────────────────
    //
    // Navigation-completion sentinel using the `<url>/favicon.ico` heuristic.
    // Pixel data is empty; a future phase will perform an async URLSession fetch.

    fn drain_favicon_updates(&self) -> Vec<FaviconUpdate> {
        #[cfg(target_os = "macos")]
        if let Ok(mut guard) = self.favicon_updates.lock() {
            return std::mem::take(&mut *guard);
        }
        Vec::new()
    }

    // ── Cursor (Gap 2 — Stubbed) ──────────────────────────────────────────────
    //
    // WKWebView does not expose cursor-change notifications via a public API.
    // TODO(phase-d): investigate KVO on WKWebView._cursorType or
    // NSCursorPopUpButton to surface cursor state.

    fn take_cursor_change(&self) -> Option<(i32, u32)> {
        None
    }

    // ── DevTools ─────────────────────────────────────────────────────────────

    // TODO(devtools): WKWebView devtools requires private API
    // (`_inspector` / `_showInspector`). Skipping for now; leave trait default.

    // ── Audio / video ────────────────────────────────────────────────────────

    fn any_audio_active(&self) -> bool {
        #[cfg(target_os = "macos")]
        return self.audio_active.load(Ordering::Relaxed);

        #[cfg(not(target_os = "macos"))]
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

    // ── Action dispatch ───────────────────────────────────────────────────────
    //
    // History/stop → existing GoBack/GoForward/Reload/Stop worker commands
    //   (guarded by #[cfg(target_os = "macos")] — on non-macOS all are no-ops).
    // Scroll → debug-log (no JS eval worker command in Phase B).
    // Zoom → existing zoom_* helpers.
    // FindNext/FindPrev → debug-log (WKFindInteraction lands in Phase C).

    // ── IME composition (Phase 8d, #86 — Phase D for macOS) ──────────────────
    //
    // Real IME on macOS requires implementing the `NSTextInputClient` protocol
    // on a custom `NSView` host.  `WKWebView`'s internal `WKContentView` already
    // implements `NSTextInputClient`, but synthesising IME events into it
    // requires patching the Cocoa responder chain (method swizzling or a forked
    // `WKWebView` subclass) — both are out of scope for Phase B.
    //
    // TODO(phase-d): Implement NSTextInputClient on a transparent overlay NSView
    //   that forwards insertText / setMarkedText / unmarkText to the underlying
    //   WKContentView.  Then wire these three methods to that overlay.
    //
    // For now: debug-log only.  Release builds optimise these calls away.

    fn ime_set_composition(&self, text: &str, cursor: Option<(usize, usize)>) {
        tracing::debug!(
            text,
            ?cursor,
            "webkit-cocoa: ime_set_composition — Phase D (NSTextInputClient not yet wired)"
        );
    }

    fn ime_commit(&self, text: &str) {
        tracing::debug!(
            text,
            "webkit-cocoa: ime_commit — Phase D (NSTextInputClient not yet wired)"
        );
    }

    fn ime_cancel(&self) {
        tracing::debug!("webkit-cocoa: ime_cancel — Phase D (NSTextInputClient not yet wired)");
    }

    // ── Edit IPC helpers (Gap 7) ──────────────────────────────────────────────
    //
    // Sentinel-driven IPC with the JS edit layer (`edit.js`).  The JS side
    // exposes `window.__buffrEditAttach`, `__buffrEditDetach`, etc.  We call
    // them via `run_js` exactly as the CEF reference implementation does.
    // See `buffr_engine::BrowserEngine::run_edit_attach` doc for rationale.

    fn run_edit_attach(&self, field_id: &str) {
        let js = format!(
            "window.__buffrEditAttach && window.__buffrEditAttach({})",
            serde_json::to_string(field_id).unwrap_or_else(|_| "\"\"".into())
        );
        let _ = self.run_js(&js);
    }

    fn run_edit_cycle(&self, forward: bool) {
        let js = format!(
            "window.__buffrEditCycle && window.__buffrEditCycle({})",
            if forward { "true" } else { "false" }
        );
        let _ = self.run_js(&js);
    }

    fn run_edit_detach(&self, field_id: &str) {
        let js = format!(
            "window.__buffrEditDetach && window.__buffrEditDetach({})",
            serde_json::to_string(field_id).unwrap_or_else(|_| "\"\"".into())
        );
        let _ = self.run_js(&js);
    }

    fn run_edit_focus(&self, field_id: &str) {
        let js = format!(
            "window.__buffrEditFocus && window.__buffrEditFocus({})",
            serde_json::to_string(field_id).unwrap_or_else(|_| "\"\"".into())
        );
        let _ = self.run_js(&js);
    }

    fn dispatch(&self, action: &buffr_modal::PageAction) {
        use buffr_modal::PageAction as A;

        match action {
            // ── Find ─────────────────────────────────────────────────────────
            A::FindNext => {
                let query = self.find_query.lock().ok().and_then(|g| g.clone());
                if let Some(q) = query {
                    tracing::debug!(query = %q, "webkit-cocoa: dispatch FindNext");
                    #[cfg(target_os = "macos")]
                    self.worker.send(Command::StartFind {
                        query: q,
                        forward: true,
                    });
                } else {
                    tracing::debug!("webkit-cocoa: FindNext — no active find query");
                }
            }
            A::FindPrev => {
                let query = self.find_query.lock().ok().and_then(|g| g.clone());
                if let Some(q) = query {
                    tracing::debug!(query = %q, "webkit-cocoa: dispatch FindPrev");
                    #[cfg(target_os = "macos")]
                    self.worker.send(Command::StartFind {
                        query: q,
                        forward: false,
                    });
                } else {
                    tracing::debug!("webkit-cocoa: FindPrev — no active find query");
                }
            }

            // ── History / reload / stop ───────────────────────────────────────
            A::HistoryBack => {
                tracing::debug!("webkit-cocoa: dispatch HistoryBack");
                #[cfg(target_os = "macos")]
                self.worker.send(Command::GoBack);
            }
            A::HistoryForward => {
                tracing::debug!("webkit-cocoa: dispatch HistoryForward");
                #[cfg(target_os = "macos")]
                self.worker.send(Command::GoForward);
            }
            A::Reload | A::ReloadHard => {
                tracing::debug!("webkit-cocoa: dispatch Reload");
                #[cfg(target_os = "macos")]
                self.worker.send(Command::Reload);
            }
            A::StopLoading => {
                tracing::debug!("webkit-cocoa: dispatch StopLoading");
                #[cfg(target_os = "macos")]
                self.worker.send(Command::Stop);
            }

            // ── Zoom ──────────────────────────────────────────────────────────
            A::ZoomIn => self.zoom_in(),
            A::ZoomOut => self.zoom_out(),
            A::ZoomReset => self.zoom_reset(),

            other => {
                tracing::debug!(
                    action = ?other,
                    "webkit-cocoa: dispatch — action not handled by this backend (no-op)"
                );
            }
        }
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
