//! [`WebKitEngine`] — Phase 2 [`BrowserEngine`] impl for WPE WebKit.
//!
//! Engine methods send [`Command`]s to the GLib worker thread via mpsc.
//! Tab state is read from the shared `Arc<Mutex<EngineState>>`.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, mpsc};

use buffr_engine::{
    BackendOpenOptions, BrowserEngine, EngineError, HintAction, HintStatus, MouseButton,
    NeutralKeyEvent, NewTabHtmlProvider, OsrFrame, OsrViewState, SharedOsrFrame,
    SharedOsrViewState, TabId, TabSummary,
    engine_id::EngineId,
    internal_server::InternalServer,
    newtab::{default_newtab_html, default_settings_html, translate_internal_url},
    popup::{
        PopupCloseSink, PopupCreateSink, PopupQueue, new_popup_close_sink, new_popup_create_sink,
        new_popup_queue,
    },
};

use super::error::WebKitError;
use super::worker::{Command, WorkerHandle, WpeKeyEvent, spawn};

// ── WebKitEngine ──────────────────────────────────────────────────────────────

/// WPE WebKit browser engine.
///
/// `Send + Sync` — all mutable state is behind `Arc<Mutex<_>>` or sent as
/// commands to the GLib worker thread.
pub struct WebKitEngine {
    #[allow(dead_code)]
    engine_id: EngineId,
    /// Shared OSR frame — written by the FDO SHM callback on the worker thread.
    frame: SharedOsrFrame,
    /// Shared OSR viewport state.
    view: SharedOsrViewState,
    /// Worker thread handle.
    worker: WorkerHandle,
    /// Popup sinks — empty in Phase 2.
    popup_queue: PopupQueue,
    popup_create_sink: PopupCreateSink,
    popup_close_sink: PopupCloseSink,
    /// Current live URL (updated via `pump_address_changes`).
    live_url: Mutex<String>,
    /// `buffr://new` HTML provider. Wired by buffr-app at registration so
    /// the page reflects current keybinds / palette / splash art. None
    /// falls back to the raw template via [`default_newtab_html`].
    newtab_html_provider: Mutex<Option<NewTabHtmlProvider>>,
    /// Optional shared loopback HTTP server. When set, `buffr://path`
    /// resolves to `http://127.0.0.1:<port>/<token>/path` instead of a
    /// `data:` URL; the server invokes per-route handlers wired by the
    /// host so internal pages get a real HTTP origin (fetch, modules,
    /// CSS imports all work) rather than the opaque data-URL origin.
    internal_server: Mutex<Option<Arc<InternalServer>>>,
    /// Per-tab mapping from TabId to the *display* URL the user typed
    /// (e.g. `buffr://new`). Tracked separately from the translated URL
    /// actually loaded into WebKit so the omnibar shows the human URL.
    display_urls: Mutex<HashMap<TabId, String>>,
}

impl WebKitEngine {
    /// Construct a new Phase 2 engine.
    ///
    /// Initialises the WPE loader, spawns the GLib worker thread, and opens
    /// the initial tab at `options.initial_url`. Equivalent to
    /// [`Self::new_with_server`] with `None`; useful when the embedder
    /// doesn't run a buffr internal-page server.
    pub fn new(options: &BackendOpenOptions<'_>) -> Result<Self, WebKitError> {
        Self::new_with_server(options, None)
    }

    /// Construct an engine and bind it to a shared [`InternalServer`] in
    /// one shot, so the worker's very first `open_tab` (fired from the
    /// GLib idle handler before the embedder can call any setter) loads
    /// `buffr://*` URLs via the server instead of falling back to a data
    /// URL. The server keeps working for every subsequent navigate.
    pub fn new_with_server(
        options: &BackendOpenOptions<'_>,
        internal_server: Option<Arc<InternalServer>>,
    ) -> Result<Self, WebKitError> {
        let (width, height) = options.initial_size;
        // Translate `buffr://*` before the worker's idle handler loads the
        // initial URL. Prefer the loopback HTTP server when available so
        // the initial tab matches the URL we'll use for subsequent navs;
        // otherwise fall back to a self-contained data: URL.
        let initial_url_owned = if let Some(rest) = options.initial_url.strip_prefix("buffr://")
            && let Some(server) = internal_server.as_ref()
        {
            server.url_for(&format!("/{rest}"))
        } else {
            translate_internal_url(
                options.initial_url,
                default_newtab_html,
                default_settings_html,
            )
            .unwrap_or_else(|| options.initial_url.to_owned())
        };
        let initial_url = initial_url_owned.as_str();

        tracing::info!("webkit: WebKitEngine::new {width}x{height}");

        let frame: SharedOsrFrame = Arc::new(Mutex::new(OsrFrame::new(width, height)));
        let view: SharedOsrViewState = Arc::new(OsrViewState::new());

        // Set initial viewport dims on the view state.
        view.width.store(width, Ordering::Relaxed);
        view.height.store(height, Ordering::Relaxed);
        if options.frame_rate > 0 {
            view.frame_rate_hz
                .store(options.frame_rate as u32, Ordering::Relaxed);
        }

        let worker = spawn(
            initial_url,
            width,
            height,
            Arc::clone(&frame),
            Arc::clone(&view),
        )?;

        Ok(Self {
            engine_id: options.engine_id.clone(),
            frame,
            view,
            worker,
            popup_queue: new_popup_queue(),
            popup_create_sink: new_popup_create_sink(),
            popup_close_sink: new_popup_close_sink(),
            live_url: Mutex::new(String::new()),
            newtab_html_provider: Mutex::new(None),
            internal_server: Mutex::new(internal_server),
            display_urls: Mutex::new({
                // The worker mints TabId(1) for the initial open_tab fired
                // from spawn's idle handler. Pre-record the display URL so
                // the omnibar shows `buffr://new` rather than the
                // translated http://127.0.0.1:.../data: URL from the very
                // first frame.
                let mut m = HashMap::new();
                m.insert(TabId(1), options.initial_url.to_owned());
                m
            }),
        })
    }

    /// Attach a shared [`InternalServer`] so future `buffr://*` navigations
    /// resolve to authenticated localhost HTTP URLs instead of opaque
    /// `data:` URLs. Idempotent; later calls replace the previous server.
    pub fn set_internal_server(&self, server: Arc<InternalServer>) {
        if let Ok(mut guard) = self.internal_server.lock() {
            *guard = Some(server);
        }
    }

    /// Wire the host-side `buffr://new` HTML provider so future buffr:// loads
    /// pick up live keybind / palette / splash content. Safe to call multiple
    /// times — overrides the previous provider.
    pub fn set_newtab_html_provider(&self, provider: NewTabHtmlProvider) {
        if let Ok(mut guard) = self.newtab_html_provider.lock() {
            *guard = Some(provider);
        }
    }

    /// Translate a `buffr://` URL into something the engine can actually
    /// load. Prefers the shared [`InternalServer`] when one is attached
    /// (real HTTP origin, supports fetch/modules) and falls back to a
    /// self-contained `data:text/html;base64,…` URL otherwise. Non-buffr
    /// URLs are returned untouched.
    fn resolve_url(&self, url: &str) -> String {
        if let Some(rest) = url.strip_prefix("buffr://") {
            // Route everything past `buffr://` straight to the server. The
            // route table on the server side is what determines whether
            // `/<rest>` resolves to a known page or 404.
            if let Ok(guard) = self.internal_server.lock()
                && let Some(server) = guard.as_ref()
            {
                return server.url_for(&format!("/{rest}"));
            }
        }
        let newtab = || {
            self.newtab_html_provider
                .lock()
                .ok()
                .and_then(|g| g.as_ref().map(|p| p()))
                .unwrap_or_else(default_newtab_html)
        };
        translate_internal_url(url, newtab, default_settings_html).unwrap_or_else(|| url.to_owned())
    }

    /// Remember that `tab_id` was opened with the display URL `original`.
    /// The omnibar reads this back via `active_tab_live_url` so users see
    /// `buffr://new` instead of the `http://127.0.0.1:.../…` or `data:…`
    /// that WebKit actually loaded.
    fn record_display_url(&self, tab_id: TabId, original: &str) {
        if let Ok(mut guard) = self.display_urls.lock() {
            guard.insert(tab_id, original.to_owned());
        }
    }

    fn forget_display_url(&self, tab_id: TabId) {
        if let Ok(mut guard) = self.display_urls.lock() {
            guard.remove(&tab_id);
        }
    }

    fn display_url_for(&self, tab_id: TabId) -> Option<String> {
        self.display_urls.lock().ok()?.get(&tab_id).cloned()
    }

    /// Send a fire-and-forget command to the worker thread.
    fn send(&self, cmd: Command) {
        if let Err(e) = self.worker.cmd_tx.try_send(cmd) {
            tracing::warn!("webkit: command send error: {e}");
        }
    }

    /// Open a tab synchronously via a reply channel. Records the original
    /// (untranslated) URL against the minted [`TabId`] so omnibar reads
    /// stay in `buffr://` space.
    fn open_tab_sync(&self, url: &str) -> Result<TabId, EngineError> {
        let original = url.to_owned();
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.send(Command::OpenTab {
            url: self.resolve_url(url),
            reply: reply_tx,
        });
        let tab_id = reply_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .map_err(|_| EngineError::Other("open_tab timed out".into()))?
            .map_err(EngineError::Other)?;
        self.record_display_url(tab_id, &original);
        Ok(tab_id)
    }
}

// ── BrowserEngine impl ────────────────────────────────────────────────────────

impl BrowserEngine for WebKitEngine {
    // ── Lifecycle ─────────────────────────────────────────────────────────────

    fn close_all_browsers(&self) {
        tracing::debug!("webkit: close_all_browsers — sending Shutdown");
        self.send(Command::Shutdown);
    }

    // ── Tabs ──────────────────────────────────────────────────────────────────

    fn open_tab(&self, url: &str) -> Result<TabId, EngineError> {
        self.open_tab_sync(url)
    }

    fn open_tab_background(&self, url: &str) -> Result<TabId, EngineError> {
        // Phase 2: single tab — background = same as foreground.
        self.open_tab(url)
    }

    fn open_tab_at(&self, url: &str, _insert_idx: usize) -> Result<TabId, EngineError> {
        self.open_tab(url)
    }

    fn close_tab(&self, id: TabId) -> Result<bool, EngineError> {
        tracing::debug!(?id, "webkit: close_tab (single-tab phase, routes through close_active)");
        self.forget_display_url(id);
        self.close_active()
    }

    fn close_active(&self) -> Result<bool, EngineError> {
        // Drop the active tab's display-URL stash now; if close_active
        // succeeds the tab is gone, and if it fails the stash is no worse
        // than slightly stale.
        if let Some(active) = self
            .worker
            .engine_state
            .lock()
            .ok()
            .and_then(|st| st.active_tab_info().map(|t| t.id))
        {
            self.forget_display_url(active);
        }
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.send(Command::CloseActive { reply: reply_tx });
        Ok(reply_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap_or(false))
    }

    fn select_tab(&self, _id: TabId) {
        tracing::debug!("webkit: select_tab stub");
    }

    fn next_tab(&self) {}
    fn prev_tab(&self) {}

    fn move_tab(&self, _from: usize, _to: usize) {}

    fn duplicate_active(&self) -> Result<TabId, EngineError> {
        let url = self.active_tab_live_url();
        self.open_tab(&url)
    }

    fn toggle_pin_active(&self) {}
    fn set_pinned(&self, _id: TabId, _pinned: bool) {}

    fn reopen_closed_tab(&self) -> Result<Option<TabId>, EngineError> {
        Ok(None)
    }

    fn closed_stack_len(&self) -> usize {
        0
    }

    fn active_tab(&self) -> Option<TabSummary> {
        let st = self.worker.engine_state.lock().ok()?;
        let info = st.active_tab_info()?;
        let mut summary = info.to_summary();
        if let Some(display) = self.display_url_for(summary.id) {
            summary.url = display;
        }
        Some(summary)
    }

    fn tabs_summary(&self) -> Vec<TabSummary> {
        let mut summaries: Vec<TabSummary> = self
            .worker
            .engine_state
            .lock()
            .map(|st| st.tabs_summary())
            .unwrap_or_default();
        for summary in &mut summaries {
            if let Some(display) = self.display_url_for(summary.id) {
                summary.url = display;
            }
        }
        summaries
    }

    fn tab_count(&self) -> usize {
        self.worker
            .engine_state
            .lock()
            .map(|st| st.tabs.len())
            .unwrap_or(0)
    }

    fn pinned_count(&self) -> usize {
        0
    }

    fn active_index(&self) -> Option<usize> {
        self.worker
            .engine_state
            .lock()
            .ok()
            .and_then(|st| st.active_idx)
    }

    // ── Navigation ────────────────────────────────────────────────────────────

    fn navigate(&self, url: &str) -> Result<(), EngineError> {
        // Update the per-tab display-URL stash before dispatching so the
        // omnibar reads the human-readable URL even if the navigation is
        // still in flight when the next paint queries us.
        if let Some(active) = self
            .worker
            .engine_state
            .lock()
            .ok()
            .and_then(|st| st.active_tab_info().map(|t| t.id))
        {
            // For non-buffr:// URLs we still record so subsequent calls
            // return the user-typed input rather than the loaded URL,
            // which may differ after redirects.
            self.record_display_url(active, url);
        }
        self.send(Command::Navigate {
            url: self.resolve_url(url),
        });
        Ok(())
    }

    fn active_tab_live_url(&self) -> String {
        // Prefer the user-typed display URL (e.g. `buffr://new`) over the
        // engine-loaded URL (e.g. the localhost+token URL or data: blob).
        let active = self
            .worker
            .engine_state
            .lock()
            .ok()
            .and_then(|st| st.active_tab_info().map(|t| (t.id, t.url.clone())));
        match active {
            Some((id, loaded)) => self.display_url_for(id).unwrap_or(loaded),
            None => String::new(),
        }
    }

    fn pump_address_changes(&self) -> bool {
        let changed = self
            .worker
            .engine_state
            .lock()
            .map(|mut st| {
                let c = st.address_changed;
                st.address_changed = false;
                c
            })
            .unwrap_or(false);
        if changed {
            let url = self.active_tab_live_url();
            if let Ok(mut lu) = self.live_url.lock() {
                *lu = url;
            }
        }
        changed
    }

    // ── Viewport ──────────────────────────────────────────────────────────────

    fn resize(&self, width: u32, height: u32) {
        self.view.width.store(width, Ordering::Relaxed);
        self.view.height.store(height, Ordering::Relaxed);
        self.send(Command::Resize { width, height });
    }

    fn set_device_scale(&self, scale: f32) {
        self.view.set_scale(scale);
    }

    fn set_frame_rate(&self, hz: u32) {
        self.view.frame_rate_hz.store(hz, Ordering::Relaxed);
    }

    fn notify_screen_info_changed(&self) {}

    fn osr_resize(&self, width: u32, height: u32) {
        self.view.width.store(width, Ordering::Relaxed);
        self.view.height.store(height, Ordering::Relaxed);
        self.send(Command::OsrResize { width, height });
    }

    // ── Input ─────────────────────────────────────────────────────────────────

    fn osr_key_event(&self, event: NeutralKeyEvent) {
        use buffr_engine::KeyEventKind;
        // Map NeutralKeyEvent → WPE keyboard event.
        // WPE doesn't distinguish RawDown vs Char — send pressed=true for both.
        let pressed = event.kind != KeyEventKind::Up;
        let ev = WpeKeyEvent {
            // Use windows_key_code as key_code (WPE keysym).
            // For proper xkb keysyms this would need a VK→XKB lookup table,
            // but for Phase 2 this gives basic Latin character input.
            key_code: event.windows_key_code as u32,
            hardware_key_code: event.native_key_code as u32,
            pressed,
            modifiers: event.modifiers,
        };
        self.send(Command::KeyEvent { ev });
    }

    fn osr_mouse_move(&self, x: i32, y: i32, modifiers: u32) {
        self.send(Command::MouseMove { x, y, modifiers });
    }

    fn osr_mouse_click(
        &self,
        x: i32,
        y: i32,
        button: MouseButton,
        mouse_up: bool,
        _click_count: i32,
        modifiers: u32,
    ) {
        let btn = match button {
            MouseButton::Left => 1,
            MouseButton::Middle => 2,
            MouseButton::Right => 3,
            MouseButton::Other(n) => n as u32,
        };
        self.send(Command::MouseClick {
            x,
            y,
            button: btn,
            pressed: !mouse_up,
            modifiers,
        });
    }

    fn osr_mouse_leave(&self, _modifiers: u32) {
        // WPE has no explicit mouse-leave — send a motion to (-1, -1) convention.
        self.send(Command::MouseMove {
            x: -1,
            y: -1,
            modifiers: 0,
        });
    }

    fn osr_mouse_wheel(&self, x: i32, y: i32, delta_x: i32, delta_y: i32, modifiers: u32) {
        self.send(Command::MouseWheel {
            x,
            y,
            delta_x,
            delta_y,
            modifiers,
        });
    }

    fn osr_focus(&self, focused: bool) {
        self.send(Command::Focus { focused });
    }

    // ── OSR state ─────────────────────────────────────────────────────────────

    fn osr_frame(&self) -> SharedOsrFrame {
        Arc::clone(&self.frame)
    }

    fn osr_view(&self) -> SharedOsrViewState {
        Arc::clone(&self.view)
    }

    fn force_repaint_active(&self) {}

    fn osr_sleep(&self, sleep: bool) {
        self.send(Command::OsrSleep { sleep });
    }

    fn osr_invalidate_view(&self) {}

    fn set_osr_wake(&self, wake: Arc<dyn Fn() + Send + Sync>) {
        self.view.set_wake(wake);
    }

    // ── Find / zoom ───────────────────────────────────────────────────────────

    fn start_find(&self, _query: &str, _forward: bool) {}
    fn stop_find(&self) {}

    fn active_zoom_level(&self) -> f64 {
        1.0
    }

    // ── Audio / video ─────────────────────────────────────────────────────────

    fn any_audio_active(&self) -> bool {
        self.worker
            .engine_state
            .lock()
            .map(|st| st.audio_active.load(Ordering::Relaxed))
            .unwrap_or(false)
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

    fn popup_resize(&self, _browser_id: i32, _width: u32, _height: u32) {}
    fn popup_close(&self, _browser_id: i32) {}

    fn popup_drain_address_changes(&self) -> Vec<(i32, String)> {
        Vec::new()
    }

    fn popup_drain_title_changes(&self) -> Vec<(i32, String)> {
        Vec::new()
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

    // ── Hint mode — stub ──────────────────────────────────────────────────────

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
}
