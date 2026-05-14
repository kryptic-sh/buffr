//! `BlinkCdpEngine` — `BrowserEngine` impl backed by headless Chromium via CDP.
//!
//! The CDP remote-debugging port is selected at runtime via an OS ephemeral-port
//! probe rather than a fixed value, so multiple engine instances can coexist
//! without port conflicts.
//!
//! # Phase 4 scope
//!
//! Implemented (minimal):
//!   - `open_tab` / `close_tab` / `close_all_browsers`
//!   - `navigate`
//!   - `osr_frame` (via `Page.captureScreenshot` polled at ~5 FPS)
//!   - `osr_mouse_click` / `osr_mouse_move` / `osr_mouse_wheel`
//!   - `osr_key_event`
//!   - `osr_resize` (via `Page.setDeviceMetricsOverride`)
//!   - `tabs_summary`, `tab_count`, `active_index`, `active_tab`
//!
//! Stubbed (return `EngineError::Unimplemented`):
//!   - All popup_* methods
//!   - hint_*, find_*, zoom_*, devtools_*, scheme_handler_*, audio_*, video_*,
//!     permissions_* methods
//!   - `duplicate_active`, `move_tab`, `reopen_closed_tab`
//!
//! # Architecture
//!
//! ```text
//! UI thread                   worker thread
//! ─────────                   ─────────────
//! BlinkCdpEngine
//!   cmd_tx ──── Command ────▶ run()
//!                              │
//!                              ├─ tungstenite WebSocket (blocking)
//!                              └─ captures screenshots → SharedOsrFrame
//! ```

use std::path::Path;
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use buffr_engine::{
    BrowserEngine, EngineError, MouseButton, NeutralKeyEvent, OsrFrame, OsrViewState,
    SharedOsrFrame, SharedOsrViewState, TabId, TabSummary,
};
use serde_json::Value;

use crate::cdp::{
    AttachToTargetParams, CdpCommand, CloseTargetParams, CreateTargetParams,
    DispatchKeyEventParams, DispatchMouseEventParams, key_event_type, mouse_button_str, next_id,
};
use crate::error::BlinkError;
use crate::subprocess::{find_chromium, pick_free_port, probe_ws_url, spawn_headless};
use crate::worker::{Command, run};
use crate::ws::WsClient;

// ── Internal tab representation ───────────────────────────────────────────────

/// Linear zoom scale factor applied via `document.body.style.zoom`.
///
/// Matches the CEF backend's 0.25-per-step increment (see
/// `buffr_cef::host::adjust_zoom` which calls `set_zoom_level(level ± 0.25)`).
///
/// CSS zoom `1.0` = 100 % (browser default).  Clamped to `[0.25, 5.0]`.
pub const ZOOM_STEP: f64 = 0.25;

/// Minimum zoom level (25 %).
pub const ZOOM_MIN: f64 = 0.25;

/// Maximum zoom level (500 %).
pub const ZOOM_MAX: f64 = 5.0;

/// Apply a zoom delta and clamp to `[ZOOM_MIN, ZOOM_MAX]`.
///
/// Pass `delta = 0.0` and `current = 1.0` to reset.
#[inline]
fn clamp_zoom(level: f64) -> f64 {
    level.clamp(ZOOM_MIN, ZOOM_MAX)
}

#[derive(Debug, Clone)]
struct CdpTab {
    id: TabId,
    target_id: String,
    session_id: String,
    url: String,
    title: String,
    /// CSS zoom factor for this tab. `1.0` = 100 % (default).
    zoom_level: f64,
}

impl CdpTab {
    fn to_summary(&self) -> TabSummary {
        TabSummary {
            id: self.id,
            browser_id: 0, // CDP has no numeric browser_id; use 0
            title: self.title.clone(),
            url: self.url.clone(),
            progress: 1.0,
            is_loading: false,
            pinned: false,
            private: false,
        }
    }
}

// ── Engine state (behind a Mutex) ─────────────────────────────────────────────

struct EngineState {
    tabs: Vec<CdpTab>,
    active: Option<TabId>,
    next_tab_id: u64,
}

impl EngineState {
    fn new() -> Self {
        Self {
            tabs: Vec::new(),
            active: None,
            next_tab_id: 1,
        }
    }

    fn mint_tab_id(&mut self) -> TabId {
        let id = TabId(self.next_tab_id);
        self.next_tab_id += 1;
        id
    }

    fn tab_by_id(&self, id: TabId) -> Option<&CdpTab> {
        self.tabs.iter().find(|t| t.id == id)
    }

    fn active_tab(&self) -> Option<&CdpTab> {
        let id = self.active?;
        self.tab_by_id(id)
    }
}

// ── Public engine struct ──────────────────────────────────────────────────────

/// Headless Chromium engine driven over Chrome DevTools Protocol.
///
/// Construct via [`BlinkCdpEngine::new`].  Each instance owns a dedicated
/// Chromium subprocess and a single CDP WebSocket connection.
pub struct BlinkCdpEngine {
    state: Arc<Mutex<EngineState>>,
    cmd_tx: SyncSender<Command>,
    osr_frame: SharedOsrFrame,
    osr_view: SharedOsrViewState,
    /// Handle to the worker thread. `None` after shutdown.
    _worker: JoinHandle<()>,
    /// Handle to the chromium subprocess.  Killed in `close_all_browsers`.
    subprocess: Arc<Mutex<Option<std::process::Child>>>,
}

impl BlinkCdpEngine {
    /// Construct a new engine instance.
    ///
    /// Locates a system Chromium binary, probes the OS for a free ephemeral
    /// port, spawns a headless subprocess on that port, waits for the CDP
    /// endpoint to become available, then connects the WebSocket and starts
    /// the worker thread.
    ///
    /// The port is selected via [`pick_free_port`] — multiple engine instances
    /// can therefore coexist without conflicts, and port 9222 is no longer
    /// special.
    ///
    /// `data_dir` is used as the Chromium user-data directory.
    pub fn new(data_dir: &Path) -> Result<Self, BlinkError> {
        let chromium = find_chromium().ok_or(BlinkError::ChromiumNotFound)?;

        // Ask the OS for a free ephemeral port.
        let port = pick_free_port()?;

        std::fs::create_dir_all(data_dir).map_err(BlinkError::SpawnFailed)?;

        let child = spawn_headless(&chromium, port, data_dir)?;

        // Wait for Chromium to start accepting connections.
        let ws_url = probe_ws_url(port, 20, Duration::from_millis(300))?;

        // Connect the browser-level WebSocket.
        let ws = WsClient::connect(&ws_url)?;

        // Build shared state.
        let osr_frame = Arc::new(Mutex::new(OsrFrame::new(1280, 800)));
        let osr_view = Arc::new(OsrViewState::new());

        // Spawn worker thread.
        let (cmd_tx, cmd_rx) = mpsc::sync_channel::<Command>(256);
        let worker_frame = Arc::clone(&osr_frame);
        let worker_view = Arc::clone(&osr_view);
        let worker = std::thread::Builder::new()
            .name("blink-cdp-worker".to_owned())
            .spawn(move || run(ws, cmd_rx, worker_frame, worker_view))
            .map_err(BlinkError::SpawnFailed)?;

        Ok(Self {
            state: Arc::new(Mutex::new(EngineState::new())),
            cmd_tx,
            osr_frame,
            osr_view,
            _worker: worker,
            subprocess: Arc::new(Mutex::new(Some(child))),
        })
    }

    // ── Internal helpers ──────────────────────────────────────────────────────

    /// Adjust the active tab's zoom by `delta` (clamped to `[ZOOM_MIN, ZOOM_MAX]`)
    /// and send a `Command::SetZoom` to the worker.
    fn adjust_zoom(&self, delta: f64) {
        let current = self
            .state
            .lock()
            .unwrap()
            .active_tab()
            .map(|t| t.zoom_level)
            .unwrap_or(1.0);
        self.apply_zoom(clamp_zoom(current + delta));
    }

    /// Set the active tab's zoom to `level` (already clamped) and send
    /// a `Command::SetZoom` to the worker.
    fn apply_zoom(&self, level: f64) {
        let session_id = {
            let mut state = self.state.lock().unwrap();
            let Some(id) = state.active else { return };
            let Some(tab) = state.tabs.iter_mut().find(|t| t.id == id) else {
                return;
            };
            tab.zoom_level = level;
            tab.session_id.clone()
        };
        tracing::debug!(level, "blink-cdp: apply_zoom");
        let _ = self.cmd_tx.try_send(Command::SetZoom { session_id, level });
    }

    /// Send a browser-level CDP command and wait for the response.
    fn browser_cmd(
        &self,
        method: &'static str,
        params: impl serde::Serialize,
    ) -> Result<Value, BlinkError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        let cmd = CdpCommand {
            id: next_id(),
            method,
            params: Some(serde_json::to_value(params).unwrap_or(Value::Null)),
            session_id: None,
        };
        self.cmd_tx
            .try_send(Command::BrowserCmd {
                cmd,
                reply: reply_tx,
            })
            .map_err(|_| BlinkError::WorkerDead)?;
        reply_rx
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| BlinkError::Timeout { method })
            .and_then(|r| r)
    }

    /// Send a session-scoped CDP command and wait for the response.
    fn session_cmd(
        &self,
        session_id: &str,
        method: &'static str,
        params: impl serde::Serialize,
    ) -> Result<Value, BlinkError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        let cmd = CdpCommand {
            id: next_id(),
            method,
            params: Some(serde_json::to_value(params).unwrap_or(Value::Null)),
            session_id: Some(session_id.to_owned()),
        };
        self.cmd_tx
            .try_send(Command::SessionCmd {
                session_id: session_id.to_owned(),
                cmd,
                reply: reply_tx,
            })
            .map_err(|_| BlinkError::WorkerDead)?;
        reply_rx
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| BlinkError::Timeout { method })
            .and_then(|r| r)
    }

    /// Create a new CDP target (page) and attach to it.
    ///
    /// Returns `(target_id, session_id)`.
    fn create_and_attach(&self, url: &str) -> Result<(String, String), BlinkError> {
        // Create target.
        let result = self.browser_cmd(
            "Target.createTarget",
            CreateTargetParams {
                url: url.to_owned(),
            },
        )?;
        let target_id = result
            .get("targetId")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                BlinkError::Protocol("missing targetId in createTarget response".into())
            })?
            .to_owned();

        // Attach.
        let result = self.browser_cmd(
            "Target.attachToTarget",
            AttachToTargetParams {
                target_id: target_id.clone(),
                flatten: true,
            },
        )?;
        let session_id = result
            .get("sessionId")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                BlinkError::Protocol("missing sessionId in attachToTarget response".into())
            })?
            .to_owned();

        Ok((target_id, session_id))
    }

    /// Internal open-tab implementation. Returns (TabId, tab_becomes_active).
    fn open_tab_internal(&self, url: &str, make_active: bool) -> Result<TabId, EngineError> {
        let (target_id, session_id) = self.create_and_attach(url).map_err(EngineError::from)?;

        // Apply initial viewport metrics.
        let (w, h) = {
            let v = &self.osr_view;
            use std::sync::atomic::Ordering;
            (
                v.width.load(Ordering::Relaxed),
                v.height.load(Ordering::Relaxed),
            )
        };
        let _ = self.session_cmd(
            &session_id,
            "Page.setDeviceMetricsOverride",
            crate::cdp::SetDeviceMetricsParams {
                width: w.max(1),
                height: h.max(1),
                device_scale_factor: 1.0,
                mobile: false,
            },
        );

        let mut state = self.state.lock().unwrap();
        let tab_id = state.mint_tab_id();
        let tab = CdpTab {
            id: tab_id,
            target_id,
            session_id: session_id.clone(),
            url: url.to_owned(),
            title: url.to_owned(),
            zoom_level: 1.0,
        };
        state.tabs.push(tab);
        if make_active || state.active.is_none() {
            state.active = Some(tab_id);
            drop(state);
            // Tell the worker to start polling screenshots for this session.
            let _ = self.cmd_tx.try_send(Command::SetActiveSession {
                session_id: Some(session_id),
            });
        }
        Ok(tab_id)
    }
}

// ── BrowserEngine impl ────────────────────────────────────────────────────────

impl BrowserEngine for BlinkCdpEngine {
    // ── Lifecycle ────────────────────────────────────────────────────────────

    fn close_all_browsers(&self) {
        tracing::debug!("blink-cdp: close_all_browsers");
        // Stop screenshot polling.
        let _ = self
            .cmd_tx
            .try_send(Command::SetActiveSession { session_id: None });
        // Shut down the worker.
        let _ = self.cmd_tx.try_send(Command::Shutdown);
        // Kill the subprocess.
        if let Ok(mut guard) = self.subprocess.lock()
            && let Some(mut child) = guard.take()
        {
            let _ = child.kill();
            let _ = child.wait();
        }
        // Clear tab state.
        if let Ok(mut state) = self.state.lock() {
            state.tabs.clear();
            state.active = None;
        }
    }

    // ── Tabs ─────────────────────────────────────────────────────────────────

    fn open_tab(&self, url: &str) -> Result<TabId, EngineError> {
        tracing::debug!(url, "blink-cdp: open_tab");
        self.open_tab_internal(url, true)
    }

    fn open_tab_background(&self, url: &str) -> Result<TabId, EngineError> {
        tracing::debug!(url, "blink-cdp: open_tab_background");
        self.open_tab_internal(url, false)
    }

    fn open_tab_at(&self, url: &str, _insert_idx: usize) -> Result<TabId, EngineError> {
        // Phase 4: index ordering not implemented; opens at end.
        tracing::debug!(url, "blink-cdp: open_tab_at (index ignored in Phase 4)");
        self.open_tab_internal(url, true)
    }

    fn close_tab(&self, id: TabId) -> Result<bool, EngineError> {
        tracing::debug!(%id, "blink-cdp: close_tab");
        let (target_id, was_active) = {
            let state = self.state.lock().unwrap();
            let tab = state.tab_by_id(id).ok_or(EngineError::TabNotFound(id))?;
            (tab.target_id.clone(), state.active == Some(id))
        };

        // Close the CDP target.
        let _ = self.browser_cmd(
            "Target.closeTarget",
            CloseTargetParams {
                target_id: target_id.clone(),
            },
        );

        let mut state = self.state.lock().unwrap();
        state.tabs.retain(|t| t.id != id);

        // Pick a new active tab if needed.
        if was_active {
            state.active = state.tabs.last().map(|t| t.id);
            let new_session = state.active.and_then(|active_id| {
                state
                    .tabs
                    .iter()
                    .find(|t| t.id == active_id)
                    .map(|t| t.session_id.clone())
            });
            drop(state);
            let _ = self.cmd_tx.try_send(Command::SetActiveSession {
                session_id: new_session,
            });
        }

        let remaining = self.state.lock().unwrap().tabs.len();
        Ok(remaining > 0)
    }

    fn close_active(&self) -> Result<bool, EngineError> {
        let id = self
            .state
            .lock()
            .unwrap()
            .active
            .ok_or(EngineError::NoActiveTab)?;
        self.close_tab(id)
    }

    fn select_tab(&self, id: TabId) {
        tracing::debug!(%id, "blink-cdp: select_tab");
        let mut state = self.state.lock().unwrap();
        if let Some(tab) = state.tab_by_id(id) {
            let session_id = tab.session_id.clone();
            state.active = Some(id);
            drop(state);
            let _ = self.cmd_tx.try_send(Command::SetActiveSession {
                session_id: Some(session_id),
            });
        }
    }

    fn next_tab(&self) {
        let (len, current_idx) = {
            let state = self.state.lock().unwrap();
            let len = state.tabs.len();
            let idx = state
                .active
                .and_then(|id| state.tabs.iter().position(|t| t.id == id))
                .unwrap_or(0);
            (len, idx)
        };
        if len == 0 {
            return;
        }
        let next_idx = (current_idx + 1) % len;
        let id = self.state.lock().unwrap().tabs[next_idx].id;
        self.select_tab(id);
    }

    fn prev_tab(&self) {
        let (len, current_idx) = {
            let state = self.state.lock().unwrap();
            let len = state.tabs.len();
            let idx = state
                .active
                .and_then(|id| state.tabs.iter().position(|t| t.id == id))
                .unwrap_or(0);
            (len, idx)
        };
        if len == 0 {
            return;
        }
        let prev_idx = if current_idx == 0 {
            len - 1
        } else {
            current_idx - 1
        };
        let id = self.state.lock().unwrap().tabs[prev_idx].id;
        self.select_tab(id);
    }

    fn move_tab(&self, _from: usize, _to: usize) {
        tracing::warn!("blink-cdp: move_tab not implemented in Phase 4");
    }

    fn duplicate_active(&self) -> Result<TabId, EngineError> {
        Err(EngineError::Unimplemented {
            method: "duplicate_active",
        })
    }

    fn toggle_pin_active(&self) {
        tracing::warn!("blink-cdp: toggle_pin_active not implemented in Phase 4");
    }

    fn set_pinned(&self, _id: TabId, _pinned: bool) {
        tracing::warn!("blink-cdp: set_pinned not implemented in Phase 4");
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
        let state = self.state.lock().unwrap();
        state.active_tab().map(|t| t.to_summary())
    }

    fn tabs_summary(&self) -> Vec<TabSummary> {
        let state = self.state.lock().unwrap();
        state.tabs.iter().map(|t| t.to_summary()).collect()
    }

    fn tab_count(&self) -> usize {
        self.state.lock().unwrap().tabs.len()
    }

    fn pinned_count(&self) -> usize {
        0
    }

    fn active_index(&self) -> Option<usize> {
        let state = self.state.lock().unwrap();
        let active = state.active?;
        state.tabs.iter().position(|t| t.id == active)
    }

    // ── Navigation ───────────────────────────────────────────────────────────

    fn navigate(&self, url: &str) -> Result<(), EngineError> {
        tracing::debug!(url, "blink-cdp: navigate");
        let session_id = {
            let state = self.state.lock().unwrap();
            state
                .active_tab()
                .ok_or(EngineError::NoActiveTab)?
                .session_id
                .clone()
        };
        // Update URL in state (optimistic; real URL comes from Page.frameNavigated events).
        {
            let mut state = self.state.lock().unwrap();
            if let Some(id) = state.active
                && let Some(tab) = state.tabs.iter_mut().find(|t| t.id == id)
            {
                tab.url = url.to_owned();
            }
        }
        let (reply_tx, reply_rx) = mpsc::channel();
        self.cmd_tx
            .try_send(Command::Navigate {
                session_id,
                url: url.to_owned(),
                reply: reply_tx,
            })
            .map_err(|_| EngineError::Other("worker channel full".into()))?;
        reply_rx
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| EngineError::Other("navigate timed out".into()))
            .and_then(|r| r.map_err(EngineError::from))
    }

    fn active_tab_live_url(&self) -> String {
        self.state
            .lock()
            .unwrap()
            .active_tab()
            .map(|t| t.url.clone())
            .unwrap_or_default()
    }

    fn pump_address_changes(&self) -> bool {
        // Phase 4: no address-change event loop; return false.
        false
    }

    // ── Viewport ─────────────────────────────────────────────────────────────

    fn resize(&self, width: u32, height: u32) {
        use std::sync::atomic::Ordering;
        self.osr_view.width.store(width, Ordering::Relaxed);
        self.osr_view.height.store(height, Ordering::Relaxed);
        self.osr_resize(width, height);
    }

    fn set_device_scale(&self, scale: f32) {
        self.osr_view.set_scale(scale);
        // Phase 4: no per-scale CDP override; Page.setDeviceMetricsOverride always
        // uses deviceScaleFactor: 1.0 for simplicity.
        tracing::debug!(
            scale,
            "blink-cdp: set_device_scale (scale stored, not forwarded to CDP)"
        );
    }

    fn set_frame_rate(&self, hz: u32) {
        use std::sync::atomic::Ordering;
        self.osr_view.frame_rate_hz.store(hz, Ordering::Relaxed);
        // Phase 4: screenshot poll rate is fixed at ~5 FPS regardless of this.
    }

    fn notify_screen_info_changed(&self) {
        // No-op in Phase 4.
    }

    fn osr_resize(&self, width: u32, height: u32) {
        tracing::debug!(width, height, "blink-cdp: osr_resize");
        let session_id = self
            .state
            .lock()
            .unwrap()
            .active_tab()
            .map(|t| t.session_id.clone());
        if let Some(sess) = session_id {
            let _ = self.cmd_tx.try_send(Command::Resize {
                session_id: sess,
                width: width.max(1),
                height: height.max(1),
            });
        }
        // Mark frame as needing a fresh paint at new dimensions.
        if let Ok(mut frame) = self.osr_frame.lock() {
            frame.needs_fresh = true;
        }
    }

    // ── Input ────────────────────────────────────────────────────────────────

    fn osr_key_event(&self, event: NeutralKeyEvent) {
        let session_id = self
            .state
            .lock()
            .unwrap()
            .active_tab()
            .map(|t| t.session_id.clone());
        let Some(session_id) = session_id else { return };

        // Build the CDP text field from the UTF-16 character.
        let text = if event.character != 0 {
            char::from_u32(event.character as u32)
                .map(|c| c.to_string())
                .unwrap_or_default()
        } else {
            String::new()
        };
        let unmodified_text = if event.unmodified_character != 0 {
            char::from_u32(event.unmodified_character as u32)
                .map(|c| c.to_string())
                .unwrap_or_default()
        } else {
            String::new()
        };

        let params = DispatchKeyEventParams {
            event_type: key_event_type(event.kind),
            windows_virtual_key_code: event.windows_key_code,
            native_virtual_key_code: event.native_key_code,
            text,
            unmodified_text,
            modifiers: event.modifiers,
            is_system_key: event.is_system_key,
        };
        let _ = self
            .cmd_tx
            .try_send(Command::KeyEvent { session_id, params });
    }

    fn osr_mouse_move(&self, x: i32, y: i32, modifiers: u32) {
        let session_id = self
            .state
            .lock()
            .unwrap()
            .active_tab()
            .map(|t| t.session_id.clone());
        let Some(session_id) = session_id else { return };
        let params = DispatchMouseEventParams {
            event_type: "mouseMoved",
            x,
            y,
            button: "none",
            click_count: 0,
            modifiers,
            delta_x: None,
            delta_y: None,
        };
        let _ = self
            .cmd_tx
            .try_send(Command::MouseEvent { session_id, params });
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
        let session_id = self
            .state
            .lock()
            .unwrap()
            .active_tab()
            .map(|t| t.session_id.clone());
        let Some(session_id) = session_id else { return };
        let event_type = if mouse_up {
            "mouseReleased"
        } else {
            "mousePressed"
        };
        let params = DispatchMouseEventParams {
            event_type,
            x,
            y,
            button: mouse_button_str(button),
            click_count,
            modifiers,
            delta_x: None,
            delta_y: None,
        };
        let _ = self
            .cmd_tx
            .try_send(Command::MouseEvent { session_id, params });
    }

    fn osr_mouse_leave(&self, _modifiers: u32) {
        // No direct CDP equivalent; ignore.
    }

    fn osr_mouse_wheel(&self, x: i32, y: i32, delta_x: i32, delta_y: i32, modifiers: u32) {
        let session_id = self
            .state
            .lock()
            .unwrap()
            .active_tab()
            .map(|t| t.session_id.clone());
        let Some(session_id) = session_id else { return };
        let params = DispatchMouseEventParams {
            event_type: "mouseWheel",
            x,
            y,
            button: "none",
            click_count: 0,
            modifiers,
            delta_x: Some(delta_x as f64),
            delta_y: Some(delta_y as f64),
        };
        let _ = self
            .cmd_tx
            .try_send(Command::MouseEvent { session_id, params });
    }

    fn osr_focus(&self, _focused: bool) {
        // No-op — CDP has no direct "focus window" command.
    }

    // ── OSR state ────────────────────────────────────────────────────────────

    fn osr_frame(&self) -> SharedOsrFrame {
        Arc::clone(&self.osr_frame)
    }

    fn osr_view(&self) -> SharedOsrViewState {
        Arc::clone(&self.osr_view)
    }

    fn force_repaint_active(&self) {
        // Phase 4: screenshot poll handles this passively.
    }

    fn osr_sleep(&self, _sleep: bool) {
        // Phase 4: no sleep/wake — poll always runs.
    }

    fn osr_invalidate_view(&self) {
        // Phase 4: no explicit invalidation needed; poll handles it.
    }

    fn set_osr_wake(&self, wake: Arc<dyn Fn() + Send + Sync>) {
        // Store in the shared view state so callers can trigger redraws.
        self.osr_view.set_wake(wake);
    }

    // ── Find / zoom ──────────────────────────────────────────────────────────

    fn start_find(&self, _query: &str, _forward: bool) {
        tracing::warn!("blink-cdp: start_find not implemented in Phase 4");
    }

    fn stop_find(&self) {
        tracing::warn!("blink-cdp: stop_find not implemented in Phase 4");
    }

    fn active_zoom_level(&self) -> f64 {
        self.state
            .lock()
            .unwrap()
            .active_tab()
            .map(|t| t.zoom_level)
            .unwrap_or(1.0)
    }

    fn zoom_in(&self) {
        self.adjust_zoom(ZOOM_STEP);
    }

    fn zoom_out(&self) {
        self.adjust_zoom(-ZOOM_STEP);
    }

    fn zoom_reset(&self) {
        self.apply_zoom(1.0);
    }

    // ── Audio / video ────────────────────────────────────────────────────────

    fn any_audio_active(&self) -> bool {
        false
    }

    fn any_video_active(&self) -> bool {
        false
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subprocess::find_chromium;

    // ── Zoom helper tests ─────────────────────────────────────────────────────

    #[test]
    fn zoom_level_clamps_to_range() {
        // Clamping below min.
        assert_eq!(clamp_zoom(0.0), ZOOM_MIN);
        assert_eq!(clamp_zoom(-1.0), ZOOM_MIN);
        // Clamping above max.
        assert_eq!(clamp_zoom(10.0), ZOOM_MAX);
        // Values inside range pass through unchanged.
        assert_eq!(clamp_zoom(1.0), 1.0);
        assert_eq!(clamp_zoom(ZOOM_MIN), ZOOM_MIN);
        assert_eq!(clamp_zoom(ZOOM_MAX), ZOOM_MAX);
        assert_eq!(clamp_zoom(2.5), 2.5);
    }

    #[test]
    fn zoom_step_constant_matches_cef() {
        // buffr-cef's `adjust_zoom` calls `set_zoom_level(level ± 0.25)`.
        // Verify blink-cdp uses the same step so both backends behave identically.
        assert!(
            (ZOOM_STEP - 0.25_f64).abs() < f64::EPSILON,
            "ZOOM_STEP must equal 0.25 to match the CEF backend"
        );
    }

    #[test]
    fn active_zoom_level_returns_tracked_value() {
        // Build a minimal EngineState with one tab and verify that
        // active_zoom_level reflects the stored zoom_level.
        let mut state = EngineState::new();
        let tab_id = state.mint_tab_id();
        state.tabs.push(CdpTab {
            id: tab_id,
            target_id: "t1".into(),
            session_id: "s1".into(),
            url: "about:blank".into(),
            title: "about:blank".into(),
            zoom_level: 1.5,
        });
        state.active = Some(tab_id);

        // active_tab() returns the tab; zoom_level should be 1.5.
        let level = state.active_tab().map(|t| t.zoom_level).unwrap_or(1.0);
        assert!(
            (level - 1.5_f64).abs() < f64::EPSILON,
            "tracked zoom level should be 1.5, got {level}"
        );

        // No active tab → default 1.0.
        state.active = None;
        let level_none = state.active_tab().map(|t| t.zoom_level).unwrap_or(1.0);
        assert!(
            (level_none - 1.0_f64).abs() < f64::EPSILON,
            "no active tab should yield 1.0, got {level_none}"
        );
    }

    // ── Chromium detection tests ──────────────────────────────────────────────

    #[test]
    fn find_chromium_no_panic() {
        // Must not panic regardless of whether Chromium is installed.
        let _result = find_chromium();
        // If found, it should be a file.
        if let Some(path) = find_chromium() {
            assert!(
                path.exists() || !path.is_absolute(),
                "resolved absolute path should exist"
            );
        }
    }

    #[test]
    fn error_when_chromium_missing() {
        // Simulate no Chromium by pointing to a non-existent data dir
        // and attempting construction. If Chromium is not installed on
        // this machine, we should get ChromiumNotFound immediately.
        // If it IS installed, we skip (don't actually spawn in unit tests).
        if find_chromium().is_some() {
            // Chromium present — skip spawning; would be an integration test.
            return;
        }
        let result = BlinkCdpEngine::new(Path::new("/tmp/buffr-blink-cdp-test"));
        match result {
            Err(BlinkError::ChromiumNotFound) => {} // expected
            Err(other) => panic!("unexpected error: {other}"),
            Ok(_) => panic!("expected error when Chromium is missing"),
        }
    }
}
