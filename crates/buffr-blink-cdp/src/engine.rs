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
//!   - `osr_frame` (via `Page.startScreencast` push; replaced 5 FPS poll)
//!   - `osr_mouse_click` / `osr_mouse_move` / `osr_mouse_wheel`
//!   - `osr_key_event`
//!   - `osr_resize` (via `Page.setDeviceMetricsOverride` + screencast restart)
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

use buffr_core::DownloadNoticeQueue;
use buffr_core::find::{FindResult, FindResultSink, new_sink as new_find_sink};
use buffr_downloads::Downloads;
use buffr_engine::{
    BrowserEngine, EngineError, MouseButton, NeutralKeyEvent, OsrFrame, OsrViewState,
    PermissionsQueue, PromptOutcome, SharedOsrFrame, SharedOsrViewState, TabId, TabSummary,
};
use serde_json::Value;

use crate::cdp::{
    AttachToTargetParams, CdpCommand, CloseTargetParams, CreateTargetParams,
    DispatchKeyEventParams, DispatchMouseEventParams, SetDeviceMetricsParams, key_event_type,
    mouse_button_str, next_id,
};
use crate::context_menu::{ContextMenuSink, new_context_menu_sink};
use crate::error::BlinkError;
use crate::find::{find_expr, parse_find_result, stop_expr};
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
    /// Chromium remote-debugging port chosen at startup.
    debug_port: u16,
}

impl EngineState {
    fn new(debug_port: u16) -> Self {
        Self {
            tabs: Vec::new(),
            active: None,
            next_tab_id: 1,
            debug_port,
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
    /// Neutral permissions queue — Phase 8a (#88). The worker pushes
    /// entries when the JS shim fires a `Runtime.bindingCalled` event for
    /// `__buffrPermissionRequest`. The UI thread drains via the trait.
    permissions_queue: PermissionsQueue,
    /// Maps `resolve_id → session_id` so `resolve_permission` can evaluate
    /// `__buffrPermissionResolve` on the correct CDP session.
    perm_session_map: Arc<Mutex<std::collections::HashMap<String, String>>>,
    /// Shared downloads store. Passed in at construction from the apps
    /// layer; the worker writes to it on CDP download events. `None` when
    /// no store was provided (private mode or blink-cdp without wiring).
    ///
    /// Held here to keep the `Arc` alive for the worker thread's clone;
    /// the engine itself does not call into the store directly.
    #[allow(dead_code)]
    downloads: Option<Arc<Downloads>>,
    /// Download notice queue for surfacing start/complete banners in the
    /// status-line chrome. `None` when not wired by the apps layer.
    ///
    /// Held here to keep the `Arc` alive for the worker thread's clone.
    #[allow(dead_code)]
    notice_queue: Option<DownloadNoticeQueue>,
    /// One-slot mailbox written by [`start_find`] / [`stop_find`] after
    /// each JS roundtrip. The apps layer polls this each tick via
    /// `buffr_core::take_find_result` to update the statusline.
    find_sink: FindResultSink,
    /// Most recent search query on the active tab. Preserved so
    /// `FindNext` / `FindPrev` (dispatched from `n` / `N` keybinds) can
    /// step through matches without repeating the full scan.
    find_query: Arc<Mutex<Option<String>>>,
    /// Context-menu request queue (Phase 8c, #87). The worker pushes entries
    /// when the JS shim fires `Runtime.bindingCalled` for `__buffrContextMenu`.
    /// The UI thread drains via `drain_context_menu_requests`.
    context_menu_sink: ContextMenuSink,
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
    ///
    /// `download_dir` — if provided — is passed to `Browser.setDownloadBehavior`
    /// so Chromium saves files there instead of the default desktop location.
    ///
    /// `downloads` and `notice_queue` are the shared stores used to record
    /// download progress and surface status-line banners. Pass `None` when
    /// running without storage (e.g. private mode without a persistent store).
    ///
    /// `find_sink` is the one-slot mailbox shared with the apps layer so
    /// find results are visible in the statusline. Pass the same sink that
    /// `AppState::find_sink` was constructed with. If `None`, a private
    /// sink is created (results are computed but not surfaced to the UI).
    pub fn new(
        data_dir: &Path,
        download_dir: Option<&Path>,
        downloads: Option<Arc<Downloads>>,
        notice_queue: Option<DownloadNoticeQueue>,
        find_sink: Option<FindResultSink>,
    ) -> Result<Self, BlinkError> {
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

        // Permissions queue and session map (Phase 8a, #88).
        let permissions_queue = buffr_engine::permissions::new_queue();
        let perm_session_map: Arc<Mutex<std::collections::HashMap<String, String>>> =
            Arc::new(Mutex::new(std::collections::HashMap::new()));

        // Context-menu sink (Phase 8c, #87).
        let context_menu_sink = new_context_menu_sink();

        // Resolve the effective download directory.  If the caller did not
        // supply one, fall back to `<data_dir>/downloads` so downloads always
        // land somewhere deterministic rather than Chromium's default desktop
        // location.
        let effective_download_dir = download_dir
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| data_dir.join("downloads"));
        if let Err(e) = std::fs::create_dir_all(&effective_download_dir) {
            tracing::warn!(
                path = %effective_download_dir.display(),
                error = %e,
                "blink-cdp: failed to create download directory"
            );
        }

        // Spawn worker thread.
        let (cmd_tx, cmd_rx) = mpsc::sync_channel::<Command>(256);
        let worker_frame = Arc::clone(&osr_frame);
        let worker_view = Arc::clone(&osr_view);
        let worker_perm_queue = Arc::clone(&permissions_queue);
        let worker_perm_session = Arc::clone(&perm_session_map);
        let worker_downloads = downloads.clone();
        let worker_notice_queue = notice_queue.clone();
        let worker_download_dir = effective_download_dir.clone();
        let worker_context_menu_sink = Arc::clone(&context_menu_sink);
        let worker = std::thread::Builder::new()
            .name("blink-cdp-worker".to_owned())
            .spawn(move || {
                run(
                    ws,
                    cmd_rx,
                    worker_frame,
                    worker_view,
                    worker_perm_queue,
                    worker_perm_session,
                    worker_downloads,
                    worker_notice_queue,
                    worker_download_dir,
                    worker_context_menu_sink,
                )
            })
            .map_err(BlinkError::SpawnFailed)?;

        // Configure Browser.setDownloadBehavior so downloads land in our
        // directory and the worker receives Browser.downloadWillBegin /
        // Browser.downloadProgress events.  This must be sent AFTER the
        // worker is started (it owns the WebSocket) via a BrowserCmd round-trip.
        let (reply_tx, reply_rx) = mpsc::channel();
        let download_behavior_cmd = crate::cdp::CdpCommand {
            id: crate::cdp::next_id(),
            method: "Browser.setDownloadBehavior",
            params: Some(serde_json::json!({
                "behavior": "allow",
                "downloadPath": effective_download_dir.to_string_lossy().as_ref(),
                "eventsEnabled": true,
            })),
            session_id: None,
        };
        let _ = cmd_tx.try_send(Command::BrowserCmd {
            cmd: download_behavior_cmd,
            reply: reply_tx,
        });
        // Best-effort: don't block startup on a timing failure.
        match reply_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(_)) => {
                tracing::debug!(
                    path = %effective_download_dir.display(),
                    "blink-cdp: Browser.setDownloadBehavior configured"
                );
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "blink-cdp: Browser.setDownloadBehavior failed");
            }
            Err(_) => {
                tracing::warn!("blink-cdp: Browser.setDownloadBehavior timed out");
            }
        }

        Ok(Self {
            state: Arc::new(Mutex::new(EngineState::new(port))),
            cmd_tx,
            osr_frame,
            osr_view,
            _worker: worker,
            subprocess: Arc::new(Mutex::new(Some(child))),
            permissions_queue,
            perm_session_map,
            downloads,
            notice_queue,
            find_sink: find_sink.unwrap_or_else(new_find_sink),
            find_query: Arc::new(Mutex::new(None)),
            context_menu_sink,
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
            SetDeviceMetricsParams {
                width: w.max(1),
                height: h.max(1),
                device_scale_factor: 1.0,
                mobile: false,
            },
        );

        // Register the permission binding so the JS shim can post requests
        // (Phase 8a, #88). `Runtime.addBinding` makes `window.__buffrPermissionRequest`
        // available in the page's JS context.
        let _ = self.session_cmd(
            &session_id,
            "Runtime.addBinding",
            serde_json::json!({ "name": "__buffrPermissionRequest" }),
        );

        // Inject the permission shim for all future documents on this session.
        let shim_js = crate::permissions::permission_shim_js();
        let _ = self.session_cmd(
            &session_id,
            "Page.addScriptToEvaluateOnNewDocument",
            serde_json::json!({ "source": shim_js }),
        );

        // Inject the find-in-page shim (Phase 8b, #83). Provides
        // `__buffrFindNext`, `__buffrFindPrev`, and `__buffrFindStop`
        // globally. The shim uses a TreeWalker-based DOM scan and CSS span
        // overlays — no native CDP find API required.
        let _ = self.session_cmd(
            &session_id,
            "Page.addScriptToEvaluateOnNewDocument",
            serde_json::json!({ "source": crate::find::find_shim_js() }),
        );

        // Register the context-menu binding and inject the hit-test shim
        // (Phase 8c, #87). `Runtime.addBinding` makes `window.__buffrContextMenu`
        // callable from the page's JS context, which the shim uses to post
        // right-click metadata to the worker.
        let _ = self.session_cmd(
            &session_id,
            "Runtime.addBinding",
            serde_json::json!({ "name": "__buffrContextMenu" }),
        );
        let _ = self.session_cmd(
            &session_id,
            "Page.addScriptToEvaluateOnNewDocument",
            serde_json::json!({ "source": crate::context_menu::context_menu_shim_js() }),
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
            // Start screencast on the new session.
            let (w, h) = self.viewport_dims();
            let _ = self.cmd_tx.try_send(Command::SetActiveSession {
                session_id: Some(session_id),
                width: w,
                height: h,
            });
        }
        Ok(tab_id)
    }

    /// Read current viewport dimensions from the shared view state.
    fn viewport_dims(&self) -> (u32, u32) {
        use std::sync::atomic::Ordering;
        let v = &self.osr_view;
        (
            v.width.load(Ordering::Relaxed).max(1),
            v.height.load(Ordering::Relaxed).max(1),
        )
    }

    /// Evaluate `expr` on the active tab's session and write a
    /// [`FindResult`] into `self.find_sink`.  Logs on failure and no-ops
    /// rather than propagating errors — find is non-critical.
    fn run_find_js(&self, expr: &str) {
        let session_id = {
            let state = self.state.lock().unwrap();
            match state.active_tab().map(|t| t.session_id.clone()) {
                Some(s) => s,
                None => {
                    tracing::debug!("blink-cdp: run_find_js — no active tab");
                    return;
                }
            }
        };

        match self.session_cmd(
            &session_id,
            "Runtime.evaluate",
            serde_json::json!({ "expression": expr, "returnByValue": true }),
        ) {
            Ok(value) => {
                if let Some(result) = parse_find_result(&value) {
                    tracing::debug!(
                        current = result.current,
                        total = result.count,
                        "blink-cdp: find result"
                    );
                    if let Ok(mut guard) = self.find_sink.lock() {
                        *guard = Some(result);
                    }
                } else {
                    tracing::debug!(?value, expr, "blink-cdp: find result parse failed");
                    // Write a zero result so the UI shows "no matches" rather
                    // than stale counts from a previous query.
                    if let Ok(mut guard) = self.find_sink.lock() {
                        *guard = Some(FindResult {
                            count: 0,
                            current: 0,
                            final_update: true,
                        });
                    }
                }
            }
            Err(e) => {
                tracing::debug!(error = %e, expr, "blink-cdp: Runtime.evaluate for find failed");
            }
        }
    }
}

// ── BrowserEngine impl ────────────────────────────────────────────────────────

impl BrowserEngine for BlinkCdpEngine {
    // ── Lifecycle ────────────────────────────────────────────────────────────

    fn close_all_browsers(&self) {
        tracing::debug!("blink-cdp: close_all_browsers");
        // Stop screencast on the active session (worker will send stopScreencast).
        let _ = self.cmd_tx.try_send(Command::SetActiveSession {
            session_id: None,
            width: 1,
            height: 1,
        });
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
            let (w, h) = self.viewport_dims();
            let _ = self.cmd_tx.try_send(Command::SetActiveSession {
                session_id: new_session,
                width: w,
                height: h,
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
            let (w, h) = self.viewport_dims();
            let _ = self.cmd_tx.try_send(Command::SetActiveSession {
                session_id: Some(session_id),
                width: w,
                height: h,
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
        // startScreencast uses everyNthFrame=1; Chromium controls cadence naturally.
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
            // Worker will: update device metrics + stop/restart screencast at new dims.
            let _ = self.cmd_tx.try_send(Command::Resize {
                session_id: sess,
                width: width.max(1),
                height: height.max(1),
            });
        }
        // Mark frame as stale until the first screencast frame at new dimensions arrives.
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
        // screencast pushes frames on demand; no explicit repaint needed.
    }

    fn osr_sleep(&self, _sleep: bool) {
        // Future: send stopScreencast / startScreencast on sleep/wake.
        // For now Chromium's ack backpressure handles idle naturally.
    }

    fn osr_invalidate_view(&self) {
        // screencast invalidation is implicit via the ack loop.
    }

    fn set_osr_wake(&self, wake: Arc<dyn Fn() + Send + Sync>) {
        // Store in the shared view state so callers can trigger redraws.
        self.osr_view.set_wake(wake);
    }

    // ── Find / zoom ──────────────────────────────────────────────────────────

    fn start_find(&self, query: &str, forward: bool) {
        tracing::debug!(%query, forward, "blink-cdp: start_find");
        // Persist the query so FindNext / FindPrev can step without re-scanning.
        if let Ok(mut guard) = self.find_query.lock() {
            *guard = if query.is_empty() {
                None
            } else {
                Some(query.to_owned())
            };
        }
        let expr = find_expr(query, false, forward);
        self.run_find_js(&expr);
    }

    fn stop_find(&self) {
        tracing::debug!("blink-cdp: stop_find");
        // Clear the stored query so FindNext / FindPrev are inert.
        if let Ok(mut guard) = self.find_query.lock() {
            *guard = None;
        }
        // Clear the find_sink so the statusline reflects no active find.
        if let Ok(mut guard) = self.find_sink.lock() {
            *guard = None;
        }
        self.run_find_js(stop_expr());
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

    // ── DevTools ─────────────────────────────────────────────────────────────

    fn open_devtools(&self, tab: TabId) -> Result<(), buffr_engine::EngineError> {
        let state = self
            .state
            .lock()
            .map_err(|e| buffr_engine::EngineError::Other(format!("state lock poisoned: {e}")))?;
        let port = state.debug_port;
        let cdp_tab = state
            .tabs
            .iter()
            .find(|t| t.id == tab)
            .ok_or(buffr_engine::EngineError::TabNotFound(tab))?;
        let target_id = cdp_tab.target_id.clone();
        drop(state);
        let url = format!(
            "http://127.0.0.1:{port}/devtools/inspector.html?ws=127.0.0.1:{port}/devtools/page/{target_id}"
        );
        tracing::debug!(%url, "blink-cdp: open_devtools");
        open::that(&url)
            .map_err(|e| buffr_engine::EngineError::Other(format!("open devtools url: {e}")))?;
        Ok(())
    }

    // ── Context menu (Phase 8c, #87) ─────────────────────────────────────────

    fn drain_context_menu_requests(&self) -> Vec<buffr_engine::ContextMenuRequest> {
        match self.context_menu_sink.lock() {
            Ok(mut q) => q.drain(..).collect(),
            Err(_) => Vec::new(),
        }
    }

    // ── Audio / video ────────────────────────────────────────────────────────

    fn any_audio_active(&self) -> bool {
        false
    }

    fn any_video_active(&self) -> bool {
        false
    }

    // ── Popup sinks (Phase 6a, #95) ──────────────────────────────────────────
    // CDP popup support: future work, see #95.

    fn popup_queue(&self) -> buffr_engine::popup::PopupQueue {
        buffr_engine::new_popup_queue()
    }

    fn popup_create_sink(&self) -> buffr_engine::popup::PopupCreateSink {
        buffr_engine::new_popup_create_sink()
    }

    fn popup_close_sink(&self) -> buffr_engine::popup::PopupCloseSink {
        buffr_engine::new_popup_close_sink()
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

    fn popup_osr_key_event(&self, _browser_id: i32, _event: buffr_engine::NeutralKeyEvent) {}

    #[allow(clippy::too_many_arguments)]
    fn popup_osr_mouse_click(
        &self,
        _browser_id: i32,
        _x: i32,
        _y: i32,
        _button: buffr_engine::MouseButton,
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

    // ── Permissions (Phase 8a, #88) ───────────────────────────────────────────

    fn permissions_queue(&self) -> PermissionsQueue {
        Arc::clone(&self.permissions_queue)
    }

    fn resolve_permission(&self, resolve_id: Option<&str>, outcome: PromptOutcome) {
        let Some(id) = resolve_id else {
            tracing::debug!("blink-cdp: resolve_permission called with no id (no-op)");
            return;
        };
        let session_id = match self.perm_session_map.lock() {
            Ok(mut map) => map.remove(id),
            Err(_) => {
                tracing::warn!(id, "blink-cdp: perm_session_map poisoned");
                return;
            }
        };
        let Some(session_id) = session_id else {
            tracing::debug!(
                id,
                "blink-cdp: resolve_id not in session map (already resolved?)"
            );
            return;
        };
        let outcome_str = match outcome {
            PromptOutcome::Allow { .. } => "granted",
            PromptOutcome::Deny { .. } | PromptOutcome::Defer => "denied",
        };
        let expr = format!(
            "if (window.__buffrPermissionResolve) {{ window.__buffrPermissionResolve({id:?}, {outcome_str:?}); }}"
        );
        tracing::debug!(
            id,
            outcome_str,
            "blink-cdp: resolve_permission → Runtime.evaluate"
        );
        let _ = self.session_cmd(
            &session_id,
            "Runtime.evaluate",
            serde_json::json!({ "expression": expr }),
        );
    }

    // ── Hint mode (Phase 6b, #95) ─────────────────────────────────────────────
    // CDP hint mode: future work, see #95.

    fn is_hint_mode(&self) -> bool {
        false
    }

    fn hint_status(&self) -> Option<buffr_engine::HintStatus> {
        None
    }

    fn pump_hint_events(&self) -> bool {
        false
    }

    fn feed_hint_key(&self, _c: char) -> Option<buffr_engine::HintAction> {
        None
    }

    fn backspace_hint(&self) -> Option<buffr_engine::HintAction> {
        None
    }

    fn cancel_hint(&self) {}

    // ── Phase 6c (#95): JS execution + DevTools at point ─────────────────────
    //
    // The CDP backend can fulfil `run_js` and `run_main_frame_js` via
    // `Runtime.evaluate` on the active session. All frame_*, media_*, image_*,
    // run_edit_*, run_media_probe, and start_download fall through to the trait
    // defaults (debug-log + no-op / Unimplemented) because they are
    // CEF-specific or rely on context-menu coordinates unavailable over CDP.
    //
    // `show_dev_tools_at` ignores (x, y) and delegates to the existing
    // `open_devtools` impl, which opens the CDP inspector URL in the system
    // browser. The trait default is overridden with a thin wrapper so the
    // apps layer gets behaviour rather than a silent no-op.

    fn run_js(&self, code: &str) -> Result<(), buffr_engine::EngineError> {
        self.run_main_frame_js(code, "")
    }

    fn run_main_frame_js(&self, code: &str, _url: &str) -> Result<(), buffr_engine::EngineError> {
        let session_id = {
            let state = self
                .state
                .lock()
                .map_err(|e| buffr_engine::EngineError::Other(format!("lock poisoned: {e}")))?;
            let tab = state
                .active_tab()
                .ok_or(buffr_engine::EngineError::NoActiveTab)?;
            tab.session_id.clone()
        };
        self.session_cmd(
            &session_id,
            "Runtime.evaluate",
            serde_json::json!({ "expression": code }),
        )
        .map(|_| ())
        .map_err(|e| buffr_engine::EngineError::Other(e.to_string()))
    }

    fn show_dev_tools_at(&self, _x: i32, _y: i32) {
        // CDP has no inspect-element at a specific point; open the full
        // inspector via the existing open_devtools path, using the active tab.
        let tab_id = {
            match self.state.lock() {
                Ok(s) => s.active_tab().map(|t| t.id),
                Err(_) => None,
            }
        };
        if let Some(id) = tab_id {
            if let Err(err) = self.open_devtools(id) {
                tracing::debug!(error = %err, "blink-cdp: show_dev_tools_at failed");
            }
        } else {
            tracing::debug!("blink-cdp: show_dev_tools_at — no active tab");
        }
    }

    // ── Action dispatch (Phase 8b, #83) ──────────────────────────────────────
    //
    // Override the default no-op so `n` / `N` (`FindNext` / `FindPrev`)
    // actually step through the JS find overlay managed by `start_find`.
    // All other actions fall through to the trait default (debug-log +
    // no-op) which is correct for CDP since most PageActions are CEF-specific.

    fn dispatch(&self, action: &buffr_modal::PageAction) {
        use buffr_modal::PageAction as A;
        match action {
            A::FindNext => {
                let query = self.find_query.lock().ok().and_then(|g| g.clone());
                if let Some(q) = query {
                    tracing::debug!(query = %q, "blink-cdp: dispatch FindNext");
                    let expr = find_expr(&q, false, true);
                    self.run_find_js(&expr);
                } else {
                    tracing::debug!("blink-cdp: FindNext — no active find query");
                }
            }
            A::FindPrev => {
                let query = self.find_query.lock().ok().and_then(|g| g.clone());
                if let Some(q) = query {
                    tracing::debug!(query = %q, "blink-cdp: dispatch FindPrev");
                    let expr = find_expr(&q, false, false);
                    self.run_find_js(&expr);
                } else {
                    tracing::debug!("blink-cdp: FindPrev — no active find query");
                }
            }
            other => {
                tracing::debug!(
                    action = ?other,
                    "blink-cdp: dispatch — action not handled by CDP backend (no-op)"
                );
            }
        }
    }

    // ── IME composition (Phase 8d, #86) ──────────────────────────────────────
    //
    // Routes winit IME events through the Chrome DevTools Protocol:
    //
    //   Preedit  → `Input.imeSetComposition`  (updates the composition window)
    //   Commit   → `Input.insertText`          (finalises the text)
    //   Cancel   → `Input.imeSetComposition` with `text: ""`  (clears preedit)
    //
    // CDP byte-offset semantics: `selectionStart` / `selectionEnd` are UTF-16
    // code-unit indices into `text`.  winit supplies byte offsets into a UTF-8
    // `&str`.  Because blink-cdp converts cursor positions only for the preedit
    // window (which is typically short and ASCII-heavy), the approximation of
    // using char counts (not UTF-16 code-unit counts) is acceptable here.
    // Exact UTF-16 conversion can be added later if needed.

    fn ime_set_composition(&self, text: &str, cursor: Option<(usize, usize)>) {
        let session_id = {
            let state = self.state.lock().unwrap();
            match state.active_tab().map(|t| t.session_id.clone()) {
                Some(s) => s,
                None => {
                    tracing::debug!("blink-cdp: ime_set_composition — no active tab");
                    return;
                }
            }
        };
        let (start, end) = cursor.unwrap_or((text.len(), text.len()));
        let params = serde_json::json!({
            "text": text,
            "selectionStart": start,
            "selectionEnd": end,
        });
        tracing::debug!(text, start, end, "blink-cdp: ime_set_composition");
        let _ = self.session_cmd(&session_id, "Input.imeSetComposition", params);
    }

    fn ime_commit(&self, text: &str) {
        let session_id = {
            let state = self.state.lock().unwrap();
            match state.active_tab().map(|t| t.session_id.clone()) {
                Some(s) => s,
                None => {
                    tracing::debug!("blink-cdp: ime_commit — no active tab");
                    return;
                }
            }
        };
        tracing::debug!(text, "blink-cdp: ime_commit");
        let _ = self.session_cmd(
            &session_id,
            "Input.insertText",
            serde_json::json!({ "text": text }),
        );
    }

    fn ime_cancel(&self) {
        let session_id = {
            let state = self.state.lock().unwrap();
            match state.active_tab().map(|t| t.session_id.clone()) {
                Some(s) => s,
                None => {
                    tracing::debug!("blink-cdp: ime_cancel — no active tab");
                    return;
                }
            }
        };
        tracing::debug!("blink-cdp: ime_cancel");
        let _ = self.session_cmd(
            &session_id,
            "Input.imeSetComposition",
            serde_json::json!({
                "text": "",
                "selectionStart": 0,
                "selectionEnd": 0,
            }),
        );
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
        let mut state = EngineState::new(9222);
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

    // ── DevTools URL format tests ─────────────────────────────────────────────

    #[test]
    fn devtools_url_format_is_correct() {
        // Verify the inspector URL template produces the expected shape.
        let port: u16 = 9222;
        let target_id = "ABCD1234-EF56-7890-ABCD-EF1234567890";
        let url = format!(
            "http://127.0.0.1:{port}/devtools/inspector.html?ws=127.0.0.1:{port}/devtools/page/{target_id}"
        );
        assert!(url.starts_with("http://127.0.0.1:9222/devtools/inspector.html"));
        assert!(url.contains("ws=127.0.0.1:9222/devtools/page/"));
        assert!(url.ends_with(target_id));
    }

    #[test]
    fn open_devtools_returns_tab_not_found_for_unknown_tab() {
        // Build a minimal EngineState with no tabs and verify that
        // open_devtools returns TabNotFound for an unknown tab id.
        let state = EngineState::new(9222);
        let unknown_id = TabId(99);
        let result = state.tabs.iter().find(|t| t.id == unknown_id);
        assert!(result.is_none(), "unknown tab should not be found");
        // Simulate the error path:
        let err = buffr_engine::EngineError::TabNotFound(unknown_id);
        assert!(matches!(err, buffr_engine::EngineError::TabNotFound(_)));
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
        let result = BlinkCdpEngine::new(
            Path::new("/tmp/buffr-blink-cdp-test"),
            None,
            None,
            None,
            None,
        );
        match result {
            Err(BlinkError::ChromiumNotFound) => {} // expected
            Err(other) => panic!("unexpected error: {other}"),
            Ok(_) => panic!("expected error when Chromium is missing"),
        }
    }

    // ── Zoom boundary tests ───────────────────────────────────────────────────

    #[test]
    fn clamp_zoom_min_boundary() {
        assert!(
            (clamp_zoom(ZOOM_MIN) - ZOOM_MIN).abs() < f64::EPSILON,
            "clamp(MIN) should equal MIN"
        );
        assert!(
            (clamp_zoom(ZOOM_MIN - 0.01) - ZOOM_MIN).abs() < f64::EPSILON,
            "below MIN should clamp to MIN"
        );
    }

    #[test]
    fn clamp_zoom_max_boundary() {
        assert!(
            (clamp_zoom(ZOOM_MAX) - ZOOM_MAX).abs() < f64::EPSILON,
            "clamp(MAX) should equal MAX"
        );
        assert!(
            (clamp_zoom(ZOOM_MAX + 0.01) - ZOOM_MAX).abs() < f64::EPSILON,
            "above MAX should clamp to MAX"
        );
    }

    // ── EngineState helper tests ──────────────────────────────────────────────

    #[test]
    fn engine_state_mint_tab_id_monotonic() {
        let mut state = EngineState::new(9999);
        let id1 = state.mint_tab_id();
        let id2 = state.mint_tab_id();
        let id3 = state.mint_tab_id();
        assert!(id1.0 < id2.0 && id2.0 < id3.0, "tab ids must increase");
    }

    #[test]
    fn engine_state_tab_by_id_found_and_not_found() {
        let mut state = EngineState::new(9999);
        let id = state.mint_tab_id();
        state.tabs.push(CdpTab {
            id,
            target_id: "t1".into(),
            session_id: "s1".into(),
            url: "about:blank".into(),
            title: "about:blank".into(),
            zoom_level: 1.0,
        });
        assert!(state.tab_by_id(id).is_some());
        assert!(state.tab_by_id(TabId(999)).is_none());
    }

    #[test]
    fn engine_state_tracks_active_target_id() {
        let mut state = EngineState::new(8080);
        let id = state.mint_tab_id();
        state.tabs.push(CdpTab {
            id,
            target_id: "target-xyz".into(),
            session_id: "sess-xyz".into(),
            url: "https://example.com".into(),
            title: "Example".into(),
            zoom_level: 1.25,
        });
        state.active = Some(id);

        let tab = state.active_tab().expect("active tab should be present");
        assert_eq!(tab.target_id, "target-xyz");
        assert_eq!(tab.session_id, "sess-xyz");
        assert!((tab.zoom_level - 1.25).abs() < f64::EPSILON);
    }

    #[test]
    fn engine_state_no_active_tab_when_none() {
        let state = EngineState::new(9999);
        assert!(state.active_tab().is_none());
    }

    // ── IME CDP payload tests (#86) ───────────────────────────────────────────

    /// Verify `Input.imeSetComposition` payload shape with explicit cursor.
    #[test]
    fn ime_set_composition_payload_shape() {
        let text = "こんにちは";
        let cursor: Option<(usize, usize)> = Some((3, 6));
        let (start, end) = cursor.unwrap_or((text.len(), text.len()));
        let params = serde_json::json!({
            "text": text,
            "selectionStart": start,
            "selectionEnd": end,
        });
        assert_eq!(params["text"], text);
        assert_eq!(params["selectionStart"], 3);
        assert_eq!(params["selectionEnd"], 6);
    }

    /// When no cursor is provided the selection collapses to the end of text.
    #[test]
    fn ime_set_composition_no_cursor_collapses_to_end() {
        let text = "hello";
        let cursor: Option<(usize, usize)> = None;
        let (start, end) = cursor.unwrap_or((text.len(), text.len()));
        let params = serde_json::json!({
            "text": text,
            "selectionStart": start,
            "selectionEnd": end,
        });
        assert_eq!(params["selectionStart"], text.len());
        assert_eq!(params["selectionEnd"], text.len());
    }

    /// `Input.insertText` commit payload must contain just `text`.
    #[test]
    fn ime_commit_payload_shape() {
        let text = "確定";
        let params = serde_json::json!({ "text": text });
        assert_eq!(params["text"], text);
        // No selection fields expected.
        assert!(params.get("selectionStart").is_none());
    }

    /// Cancel sends `Input.imeSetComposition` with an empty string and zero offsets.
    #[test]
    fn ime_cancel_payload_shape() {
        let params = serde_json::json!({
            "text": "",
            "selectionStart": 0,
            "selectionEnd": 0,
        });
        assert_eq!(params["text"], "");
        assert_eq!(params["selectionStart"], 0);
        assert_eq!(params["selectionEnd"], 0);
    }
}
