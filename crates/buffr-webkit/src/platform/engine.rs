//! [`WebKitEngine`] — Phase 2 [`BrowserEngine`] impl for WPE WebKit.
//!
//! Engine methods send [`Command`]s to the GLib worker thread via mpsc.
//! Tab state is read from the shared `Arc<Mutex<EngineState>>`.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, mpsc};

use buffr_engine::{
    BackendOpenOptions, BrowserEngine, EngineError, HintAction, HintStatus, MouseButton,
    NeutralKeyEvent, NewTabHtmlProvider, OsrFrame, OsrViewState, SharedOsrFrame,
    SharedOsrViewState, TabId, TabSummary,
    engine_id::EngineId,
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
}

impl WebKitEngine {
    /// Construct a new Phase 2 engine.
    ///
    /// Initialises the WPE loader, spawns the GLib worker thread, and opens
    /// the initial tab at `options.initial_url`.
    pub fn new(options: &BackendOpenOptions<'_>) -> Result<Self, WebKitError> {
        let (width, height) = options.initial_size;
        // Translate `buffr://*` before the worker's idle handler loads the
        // initial URL — the host-supplied newtab HTML provider isn't wired
        // until after `new` returns, so we fall back to default templates
        // here. Without this, the very first tab loads raw `buffr://new`
        // and renders the "URL can't be shown" error page.
        let initial_url_owned = translate_internal_url(
            options.initial_url,
            default_newtab_html,
            default_settings_html,
        )
        .unwrap_or_else(|| options.initial_url.to_owned());
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
        })
    }

    /// Wire the host-side `buffr://new` HTML provider so future buffr:// loads
    /// pick up live keybind / palette / splash content. Safe to call multiple
    /// times — overrides the previous provider.
    pub fn set_newtab_html_provider(&self, provider: NewTabHtmlProvider) {
        if let Ok(mut guard) = self.newtab_html_provider.lock() {
            *guard = Some(provider);
        }
    }

    /// Translate a `buffr://` URL to a data: URL the engine can load.
    /// Returns the input untouched for non-internal URLs.
    fn resolve_url(&self, url: &str) -> String {
        let newtab = || {
            self.newtab_html_provider
                .lock()
                .ok()
                .and_then(|g| g.as_ref().map(|p| p()))
                .unwrap_or_else(default_newtab_html)
        };
        translate_internal_url(url, newtab, default_settings_html).unwrap_or_else(|| url.to_owned())
    }

    /// Send a fire-and-forget command to the worker thread.
    fn send(&self, cmd: Command) {
        if let Err(e) = self.worker.cmd_tx.try_send(cmd) {
            tracing::warn!("webkit: command send error: {e}");
        }
    }

    /// Open a tab synchronously via a reply channel.
    fn open_tab_sync(&self, url: &str) -> Result<TabId, EngineError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.send(Command::OpenTab {
            url: self.resolve_url(url),
            reply: reply_tx,
        });
        reply_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .map_err(|_| EngineError::Other("open_tab timed out".into()))?
            .map_err(|e| EngineError::Other(e))
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

    fn close_tab(&self, _id: TabId) -> Result<bool, EngineError> {
        tracing::debug!("webkit: close_tab stub (single-tab phase)");
        Ok(true)
    }

    fn close_active(&self) -> Result<bool, EngineError> {
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
        Some(info.to_summary())
    }

    fn tabs_summary(&self) -> Vec<TabSummary> {
        self.worker
            .engine_state
            .lock()
            .map(|st| st.tabs_summary())
            .unwrap_or_default()
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
        self.send(Command::Navigate {
            url: self.resolve_url(url),
        });
        Ok(())
    }

    fn active_tab_live_url(&self) -> String {
        self.worker
            .engine_state
            .lock()
            .ok()
            .and_then(|st| st.active_tab_info().map(|t| t.url.clone()))
            .unwrap_or_default()
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
