//! `FirefoxCdpEngine` — `BrowserEngine` impl backed by headless Firefox via CDP.
//!
//! Firefox supports a subset of Chrome DevTools Protocol (CDP) via its Remote
//! Agent. This engine drives Firefox over that subset.
//!
//! # Key Firefox differences from blink-cdp
//!
//! - **No `Page.startScreencast`**: Firefox CDP has no streaming screencast.
//!   OSR uses a `Page.captureScreenshot` poll loop at 4 fps (Phase B hard-code;
//!   tunable in Phase C). This is the canonical Firefox-CDP OSR approach.
//! - **Profile not user-data-dir**: Firefox requires `--profile <dir>` with a
//!   pre-initialised `prefs.js` to enable the Remote Agent and suppress the
//!   first-run wizard.
//! - **`Emulation.setDeviceMetricsOverride`** instead of
//!   `Page.setDeviceMetricsOverride` for viewport.
//! - **`Browser.grantPermissions`** is deferred (not supported in Firefox CDP).
//!
//! # Phase B scope
//!
//! Implemented:
//! - `open_tab`, `open_tab_background`, `open_tab_at`, `close_tab`, `close_active`
//! - `select_tab`, `next_tab`, `prev_tab`
//! - `navigate`, `go_back`, `go_forward`, `reload`, `stop`
//! - `active_tab`, `tabs_summary`, `tab_count`, `active_index`, `active_tab_live_url`
//! - `osr_resize`, `osr_frame`, `osr_view`
//! - `osr_key_event`, `osr_mouse_move`, `osr_mouse_click`, `osr_mouse_leave`,
//!   `osr_mouse_wheel`
//! - `pump_address_changes`
//!
//! Still `Unimplemented`:
//! - popup_*, hint_*, find_*, zoom_*, devtools_*, scheme_handler_*, audio_*, video_*

use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use buffr_engine::{
    BrowserEngine, EngineError, HintAction, HintStatus, MouseButton, NeutralKeyEvent, OsrFrame,
    OsrViewState, SharedOsrFrame, SharedOsrViewState, TabId, TabSummary,
    popup::{
        PopupCloseSink, PopupCreateSink, PopupQueue, new_popup_close_sink, new_popup_create_sink,
        new_popup_queue,
    },
};

use crate::cdp::{
    AttachToTargetParams, CdpCommand, CloseTargetParams, CreateTargetParams,
    DispatchKeyEventParams, DispatchMouseEventParams, SetDeviceMetricsParams, key_event_type,
    mouse_button_str, next_id,
};
use crate::error::FirefoxError;
use crate::subprocess::{find_firefox, init_profile, pick_free_port, probe_ws_url, spawn_headless};
use crate::worker::{Command, TitleMap, UrlUpdateSink, new_title_map, new_url_update_sink, run};
use crate::ws::WsClient;

// ── Internal tab representation ───────────────────────────────────────────────

#[derive(Debug, Clone)]
struct FfTab {
    id: TabId,
    target_id: String,
    session_id: String,
    url: String,
    title: String,
    /// CSS-zoom level (multiplier on `document.body.style.zoom`). Phase C
    /// uses JS injection — Firefox CDP exposes no native page-scale API
    /// equivalent to Chromium's `Emulation.setPageScaleFactor` for headless.
    zoom: f64,
}

// ── Navigation-history cache ──────────────────────────────────────────────────

/// Cached `Page.getNavigationHistory` result for a single session.
///
/// Keyed by `session_id`. Invalidated on any navigation event for that session
/// (including `navigate()` calls and `pump_address_changes` URL updates).
type NavHistoryCache = HashMap<String, crate::cdp::NavigationHistoryResult>;

impl FfTab {
    fn to_summary(&self, is_loading: bool) -> TabSummary {
        TabSummary {
            id: self.id,
            browser_id: 0,
            title: self.title.clone(),
            url: self.url.clone(),
            progress: if is_loading { 0.5 } else { 1.0 },
            is_loading,
            pinned: false,
            private: false,
        }
    }
}

// ── Engine state ──────────────────────────────────────────────────────────────

struct EngineState {
    tabs: Vec<FfTab>,
    active: Option<TabId>,
    next_tab_id: u64,
    /// CDP remote-debugging port. Kept for devtools URL construction (Phase C).
    #[allow(dead_code)]
    debug_port: u16,
    /// Maps `target_id → original_url` for tabs navigated via internal schemes.
    original_urls: HashMap<String, String>,
    /// Stack of URLs from recently closed tabs (most-recent last).
    ///
    /// Pushed by `close_tab`; popped by `reopen_closed_tab`. Mirrors the
    /// browser's "recently closed" undo stack.
    closed_stack: Vec<String>,
}

impl EngineState {
    fn new(debug_port: u16) -> Self {
        Self {
            tabs: Vec::new(),
            active: None,
            next_tab_id: 1,
            debug_port,
            original_urls: HashMap::new(),
            closed_stack: Vec::new(),
        }
    }

    fn mint_tab_id(&mut self) -> TabId {
        let id = TabId(self.next_tab_id);
        self.next_tab_id += 1;
        id
    }

    fn tab_by_id(&self, id: TabId) -> Option<&FfTab> {
        self.tabs.iter().find(|t| t.id == id)
    }

    fn active_tab(&self) -> Option<&FfTab> {
        let id = self.active?;
        self.tab_by_id(id)
    }
}

// ── Public engine struct ──────────────────────────────────────────────────────

/// Return the display URL for a tab, preferring any stashed original URL.
fn display_url_for<'a>(tab: &'a FfTab, original_urls: &'a HashMap<String, String>) -> &'a str {
    original_urls
        .get(&tab.target_id)
        .map(String::as_str)
        .unwrap_or(tab.url.as_str())
}

/// Headless Firefox engine driven over Chrome DevTools Protocol.
///
/// Construct via [`FirefoxCdpEngine::new`]. Each instance owns a dedicated
/// Firefox subprocess and a single CDP WebSocket connection.
pub struct FirefoxCdpEngine {
    state: Arc<Mutex<EngineState>>,
    cmd_tx: SyncSender<Command>,
    osr_frame: SharedOsrFrame,
    osr_view: SharedOsrViewState,
    worker: Option<JoinHandle<()>>,
    subprocess: Arc<Mutex<Option<std::process::Child>>>,
    popup_queue: PopupQueue,
    popup_create_sink: PopupCreateSink,
    popup_close_sink: PopupCloseSink,
    url_update_sink: UrlUpdateSink,
    loading_state: Arc<Mutex<HashMap<String, bool>>>,
    nav_count: Arc<Mutex<HashMap<String, usize>>>,
    title_map: TitleMap,
    /// Cache of `Page.getNavigationHistory` results keyed by `session_id`.
    ///
    /// Entries are invalidated whenever a navigation event is observed for the
    /// corresponding session (in `navigate()` and `pump_address_changes()`).
    /// `can_go_back` / `can_go_forward` populate the cache on cache-miss.
    nav_history_cache: Mutex<NavHistoryCache>,
    /// Last query passed to `start_find`. Used by `dispatch` to re-run
    /// `FindNext` / `FindPrev` without requiring the caller to re-supply the
    /// query string. Cleared by `stop_find`.
    find_query: Mutex<Option<String>>,
}

impl FirefoxCdpEngine {
    /// Construct a new engine instance.
    ///
    /// Locates a Firefox binary, probes the OS for a free port, initialises
    /// the profile directory with the required `prefs.js`, spawns Firefox
    /// in headless mode, waits for the CDP endpoint, then connects the
    /// WebSocket and starts the worker thread.
    ///
    /// `profile_dir` is used as the Firefox profile directory (`--profile`).
    ///
    /// `private` — when `true`, passes `--private-window` to Firefox.
    pub fn new(profile_dir: &Path, private: bool) -> Result<Self, FirefoxError> {
        let firefox = find_firefox().ok_or(FirefoxError::FirefoxNotFound)?;

        let port = pick_free_port()?;

        init_profile(profile_dir)?;

        let child = spawn_headless(&firefox, port, profile_dir, private)?;

        let ws_url = probe_ws_url(
            port,
            crate::subprocess::WS_PROBE_MAX_RETRIES,
            Duration::from_millis(crate::subprocess::WS_PROBE_INTERVAL_MS),
        )?;

        let ws = WsClient::connect(&ws_url)?;

        let osr_frame = Arc::new(Mutex::new(OsrFrame::new(1280, 800)));
        let osr_view = Arc::new(OsrViewState::new());

        let url_update_sink = new_url_update_sink();
        let loading_state: Arc<Mutex<HashMap<String, bool>>> = Arc::new(Mutex::new(HashMap::new()));
        let nav_count: Arc<Mutex<HashMap<String, usize>>> = Arc::new(Mutex::new(HashMap::new()));
        let title_map = new_title_map();

        let (cmd_tx, cmd_rx) = mpsc::sync_channel::<Command>(crate::worker::WORKER_CMD_CHANNEL_CAP);

        let worker_frame = Arc::clone(&osr_frame);
        let worker_view = Arc::clone(&osr_view);
        let worker_url_sink = Arc::clone(&url_update_sink);
        let worker_loading = Arc::clone(&loading_state);
        let worker_nav = Arc::clone(&nav_count);
        let worker_title = Arc::clone(&title_map);

        let worker = std::thread::Builder::new()
            .name("firefox-cdp-worker".to_owned())
            .spawn(move || {
                run(
                    ws,
                    cmd_rx,
                    worker_frame,
                    worker_view,
                    worker_url_sink,
                    worker_loading,
                    worker_nav,
                    worker_title,
                )
            })
            .map_err(FirefoxError::SpawnFailed)?;

        Ok(Self {
            state: Arc::new(Mutex::new(EngineState::new(port))),
            cmd_tx,
            osr_frame,
            osr_view,
            worker: Some(worker),
            subprocess: Arc::new(Mutex::new(Some(child))),
            popup_queue: new_popup_queue(),
            popup_create_sink: new_popup_create_sink(),
            popup_close_sink: new_popup_close_sink(),
            url_update_sink,
            loading_state,
            nav_count,
            title_map,
            nav_history_cache: Mutex::new(HashMap::new()),
            find_query: Mutex::new(None),
        })
    }

    // ── Internal helpers ──────────────────────────────────────────────────────

    fn lock_state(&self) -> std::sync::MutexGuard<'_, EngineState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Send a browser-level CDP command and wait for the response.
    fn browser_cmd(
        &self,
        method: &'static str,
        params: impl serde::Serialize,
    ) -> Result<serde_json::Value, FirefoxError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        let cmd = CdpCommand {
            id: next_id(),
            method,
            params: Some(serde_json::to_value(params).unwrap_or(serde_json::Value::Null)),
            session_id: None,
        };
        self.cmd_tx
            .try_send(Command::BrowserCmd {
                cmd,
                reply: reply_tx,
            })
            .map_err(|_| FirefoxError::WorkerDead)?;
        reply_rx
            .recv_timeout(Duration::from_secs(
                crate::worker::CDP_RESPONSE_TIMEOUT_SECS,
            ))
            .map_err(|_| FirefoxError::Timeout { method })
            .and_then(|r| r)
    }

    /// Send a session-scoped CDP command and wait for the response.
    fn session_cmd(
        &self,
        session_id: &str,
        method: &'static str,
        params: impl serde::Serialize,
    ) -> Result<serde_json::Value, FirefoxError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        let cmd = CdpCommand {
            id: next_id(),
            method,
            params: Some(serde_json::to_value(params).unwrap_or(serde_json::Value::Null)),
            session_id: Some(session_id.to_owned()),
        };
        self.cmd_tx
            .try_send(Command::SessionCmd {
                session_id: session_id.to_owned(),
                cmd,
                reply: reply_tx,
            })
            .map_err(|_| FirefoxError::WorkerDead)?;
        reply_rx
            .recv_timeout(Duration::from_secs(
                crate::worker::CDP_RESPONSE_TIMEOUT_SECS,
            ))
            .map_err(|_| FirefoxError::Timeout { method })
            .and_then(|r| r)
    }

    /// Create a new CDP target (page) and attach to it.
    ///
    /// Returns `(target_id, session_id)`.
    fn create_and_attach(&self, url: &str) -> Result<(String, String), FirefoxError> {
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
                FirefoxError::Protocol("missing targetId in createTarget response".into())
            })?
            .to_owned();

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
                FirefoxError::Protocol("missing sessionId in attachToTarget response".into())
            })?
            .to_owned();

        Ok((target_id, session_id))
    }

    /// Internal open-tab implementation.
    ///
    /// `make_active` — whether to switch to the new tab immediately.
    /// `insert_idx` — when `Some(i)`, insert at that position instead of pushing.
    fn open_tab_internal(
        &self,
        url: &str,
        make_active: bool,
        insert_idx: Option<usize>,
    ) -> Result<TabId, EngineError> {
        // Open with about:blank first so we can set up metrics before loading.
        let (target_id, session_id) = self
            .create_and_attach("about:blank")
            .map_err(EngineError::from)?;

        // Helper: run a session setup command, warn but continue on failure.
        let setup_cmd = |engine: &Self, method: &'static str, params: serde_json::Value| {
            if let Err(e) = engine.session_cmd(&session_id, method, params) {
                tracing::warn!(
                    target_id = %target_id,
                    method,
                    error = %e,
                    "firefox-cdp: session setup command failed"
                );
            }
        };

        // Enable Page domain for frameNavigated events.
        setup_cmd(self, "Page.enable", serde_json::json!({}));
        // Enable Runtime domain for Runtime.evaluate.
        setup_cmd(self, "Runtime.enable", serde_json::json!({}));

        // Enable lifecycle events for loading state tracking.
        setup_cmd(
            self,
            "Page.setLifecycleEventsEnabled",
            serde_json::json!({ "enabled": true }),
        );
        if let Ok(mut map) = self.loading_state.lock() {
            map.insert(session_id.clone(), true);
        }

        // Apply initial viewport metrics.
        let (w, h) = self.viewport_dims();
        setup_cmd(
            self,
            "Emulation.setDeviceMetricsOverride",
            serde_json::to_value(SetDeviceMetricsParams {
                width: w.max(1),
                height: h.max(1),
                device_scale_factor: 1.0,
                mobile: false,
            })
            .unwrap_or(serde_json::Value::Null),
        );

        // Navigate to the real URL.
        tracing::debug!(url, "firefox-cdp: navigating to real URL");
        let (nav_reply_tx, nav_reply_rx) = mpsc::channel();
        self.cmd_tx
            .try_send(Command::Navigate {
                session_id: session_id.clone(),
                url: url.to_owned(),
                reply: nav_reply_tx,
            })
            .map_err(|_| EngineError::Other("worker channel full during open_tab".into()))?;
        match nav_reply_rx.recv_timeout(Duration::from_secs(
            crate::worker::CDP_RESPONSE_TIMEOUT_SECS,
        )) {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                tracing::warn!(url, error = %e, "firefox-cdp: initial navigation error (continuing)");
            }
            Err(_) => {
                tracing::warn!(
                    url,
                    "firefox-cdp: initial navigation timed out (continuing)"
                );
            }
        }

        let mut state = self.lock_state();
        let tab_id = state.mint_tab_id();
        let tab = FfTab {
            id: tab_id,
            target_id: target_id.clone(),
            session_id: session_id.clone(),
            url: url.to_owned(),
            title: url.to_owned(),
            zoom: 1.0,
        };
        match insert_idx {
            Some(idx) => {
                let clamped = idx.min(state.tabs.len());
                state.tabs.insert(clamped, tab);
            }
            None => state.tabs.push(tab),
        }
        if make_active || state.active.is_none() {
            state.active = Some(tab_id);
            drop(state);
            let _ = self.cmd_tx.try_send(Command::SetActiveSession {
                session_id: Some(session_id),
            });
        }
        Ok(tab_id)
    }

    /// Query (or return a cached) `Page.getNavigationHistory` result.
    ///
    /// On cache-hit the CDP round-trip is skipped. On cache-miss the result is
    /// stored for subsequent calls. Errors during the CDP call are silently
    /// treated as "no history" — the cache is **not** populated on error so the
    /// next call will retry.
    fn nav_history(&self, session_id: &str) -> Option<crate::cdp::NavigationHistoryResult> {
        {
            let cache = self
                .nav_history_cache
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if let Some(cached) = cache.get(session_id) {
                return Some(cached.clone());
            }
        }

        // Cache miss — query Firefox.
        let result = self
            .session_cmd(
                session_id,
                "Page.getNavigationHistory",
                serde_json::json!({}),
            )
            .ok()?;

        let history: crate::cdp::NavigationHistoryResult = serde_json::from_value(result).ok()?;

        {
            let mut cache = self
                .nav_history_cache
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            cache.insert(session_id.to_owned(), history.clone());
        }

        Some(history)
    }

    /// Apply a CSS zoom level to the active tab via Runtime.evaluate.
    ///
    /// Firefox CDP exposes no Emulation.setPageScaleFactor for headless
    /// pages, so we inject `document.body.style.zoom` over the wire.
    /// Updates the cached `FfTab.zoom` on success so `active_zoom_level`
    /// reads it without round-tripping CDP.
    fn apply_zoom(&self, level: f64) -> Result<(), FirefoxError> {
        let clamped = level.clamp(0.25, 5.0);
        let Some((tab_id, session_id)) = ({
            let state = self.lock_state();
            state
                .active
                .and_then(|id| state.tabs.iter().find(|t| t.id == id).cloned())
                .map(|t| (t.id, t.session_id))
        }) else {
            return Ok(());
        };
        self.session_cmd(
            &session_id,
            "Runtime.evaluate",
            serde_json::json!({
                "expression": format!("document.body.style.zoom = '{clamped}'"),
                "returnByValue": true,
            }),
        )?;
        // Mirror into cache.
        let mut state = self.lock_state();
        if let Some(t) = state.tabs.iter_mut().find(|t| t.id == tab_id) {
            t.zoom = clamped;
        }
        Ok(())
    }

    /// Invalidate the nav-history cache for the given session.
    fn invalidate_nav_history(&self, session_id: &str) {
        let mut cache = self
            .nav_history_cache
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        cache.remove(session_id);
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
}

// ── Drop ──────────────────────────────────────────────────────────────────────

impl Drop for FirefoxCdpEngine {
    fn drop(&mut self) {
        tracing::debug!("firefox-cdp: Drop — shutting down worker and subprocess");

        if let Err(e) = self.cmd_tx.send(Command::Shutdown) {
            tracing::debug!(error = %e, "firefox-cdp: Drop — Shutdown send failed (worker already gone)");
        }

        if let Some(handle) = self.worker.take()
            && let Err(e) = handle.join()
        {
            tracing::warn!("firefox-cdp: Drop — worker thread panicked: {:?}", e);
        }

        if let Ok(mut guard) = self.subprocess.lock()
            && let Some(mut child) = guard.take()
        {
            let _ = child.kill();
            let _ = child.wait();
            tracing::debug!("firefox-cdp: Drop — Firefox subprocess killed");
        }
    }
}

// ── BrowserEngine impl ────────────────────────────────────────────────────────

impl BrowserEngine for FirefoxCdpEngine {
    // ── Lifecycle ────────────────────────────────────────────────────────────

    fn close_all_browsers(&self) {
        tracing::debug!("firefox-cdp: close_all_browsers");
        let _ = self
            .cmd_tx
            .try_send(Command::SetActiveSession { session_id: None });
        if let Err(e) = self.cmd_tx.send(Command::Shutdown) {
            tracing::warn!(error = %e, "firefox-cdp: close_all_browsers — Shutdown send failed");
        }
        if let Ok(mut guard) = self.subprocess.lock()
            && let Some(mut child) = guard.take()
        {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Ok(mut state) = self.state.lock() {
            state.tabs.clear();
            state.active = None;
            state.original_urls.clear();
        }
    }

    // ── Tabs ─────────────────────────────────────────────────────────────────

    fn open_tab(&self, url: &str) -> Result<TabId, EngineError> {
        tracing::debug!(url, "firefox-cdp: open_tab");
        self.open_tab_internal(url, true, None)
    }

    fn open_tab_background(&self, url: &str) -> Result<TabId, EngineError> {
        tracing::debug!(url, "firefox-cdp: open_tab_background");
        self.open_tab_internal(url, false, None)
    }

    fn open_tab_at(&self, url: &str, insert_idx: usize) -> Result<TabId, EngineError> {
        tracing::debug!(url, insert_idx, "firefox-cdp: open_tab_at");
        self.open_tab_internal(url, true, Some(insert_idx))
    }

    fn close_tab(&self, id: TabId) -> Result<bool, EngineError> {
        tracing::debug!(%id, "firefox-cdp: close_tab");
        let (target_id, session_id, was_active, closing_url) = {
            let state = self.lock_state();
            let tab = state.tab_by_id(id).ok_or(EngineError::TabNotFound(id))?;
            let url = display_url_for(tab, &state.original_urls).to_owned();
            (
                tab.target_id.clone(),
                tab.session_id.clone(),
                state.active == Some(id),
                url,
            )
        };

        // Close the CDP target.
        if let Err(e) = self.browser_cmd(
            "Target.closeTarget",
            CloseTargetParams {
                target_id: target_id.clone(),
            },
        ) {
            tracing::warn!(%id, error = %e, "firefox-cdp: close_tab — closeTarget failed (continuing)");
        }

        // Clean up per-session state.
        if let Ok(mut m) = self.loading_state.lock() {
            m.remove(&session_id);
        }
        if let Ok(mut m) = self.nav_count.lock() {
            m.remove(&session_id);
        }
        self.invalidate_nav_history(&session_id);

        let mut state = self.lock_state();
        state.tabs.retain(|t| t.id != id);
        state.original_urls.remove(&target_id);

        // Push the closing URL onto the reopen stack (skip internal-only URLs).
        if !closing_url.is_empty() && closing_url != "about:blank" && closing_url != "about:newtab"
        {
            state.closed_stack.push(closing_url);
        }

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

        let remaining = self.lock_state().tabs.len();
        Ok(remaining > 0)
    }

    fn close_active(&self) -> Result<bool, EngineError> {
        let id = self
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .active
            .ok_or(EngineError::NoActiveTab)?;
        self.close_tab(id)
    }

    fn select_tab(&self, id: TabId) {
        tracing::debug!(%id, "firefox-cdp: select_tab");
        let mut state = self.lock_state();
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
            let state = self.lock_state();
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
        let id = self.lock_state().tabs[next_idx].id;
        self.select_tab(id);
    }

    fn prev_tab(&self) {
        let (len, current_idx) = {
            let state = self.lock_state();
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
        let id = self.lock_state().tabs[prev_idx].id;
        self.select_tab(id);
    }

    fn move_tab(&self, _from: usize, _to: usize) {
        tracing::warn!("firefox-cdp: move_tab not implemented in Phase B");
    }

    fn duplicate_active(&self) -> Result<TabId, EngineError> {
        tracing::debug!("firefox-cdp: duplicate_active");
        let url = {
            let state = self.lock_state();
            let tab = state.active_tab().ok_or(EngineError::NoActiveTab)?;
            display_url_for(tab, &state.original_urls).to_owned()
        };
        // Open as a new foreground tab at the same URL.
        self.open_tab_internal(&url, true, None)
    }

    fn toggle_pin_active(&self) {
        tracing::warn!("firefox-cdp: toggle_pin_active not implemented in Phase B");
    }

    fn set_pinned(&self, _id: TabId, _pinned: bool) {
        tracing::warn!("firefox-cdp: set_pinned not implemented in Phase B");
    }

    fn reopen_closed_tab(&self) -> Result<Option<TabId>, EngineError> {
        tracing::debug!("firefox-cdp: reopen_closed_tab");
        let url = {
            let mut state = self.lock_state();
            state.closed_stack.pop()
        };
        match url {
            None => Ok(None),
            Some(u) => {
                let tab_id = self.open_tab_internal(&u, true, None)?;
                Ok(Some(tab_id))
            }
        }
    }

    fn closed_stack_len(&self) -> usize {
        self.lock_state().closed_stack.len()
    }

    fn active_tab(&self) -> Option<TabSummary> {
        let state = self.lock_state();
        let loading = self.loading_state.lock().ok();
        let titles = self.title_map.lock().ok();
        state.active_tab().map(|t| {
            let is_loading = loading
                .as_ref()
                .and_then(|m| m.get(&t.session_id))
                .copied()
                .unwrap_or(false);
            let display = display_url_for(t, &state.original_urls).to_owned();
            let mut summary = t.to_summary(is_loading);
            summary.url = display;
            if let Some(live_title) = titles
                .as_ref()
                .and_then(|m| m.get(&t.target_id))
                .filter(|s| !s.is_empty())
            {
                summary.title = live_title.clone();
            }
            summary
        })
    }

    fn tabs_summary(&self) -> Vec<TabSummary> {
        let state = self.lock_state();
        let loading = self.loading_state.lock().ok();
        let titles = self.title_map.lock().ok();
        state
            .tabs
            .iter()
            .map(|t| {
                let is_loading = loading
                    .as_ref()
                    .and_then(|m| m.get(&t.session_id))
                    .copied()
                    .unwrap_or(false);
                let display = display_url_for(t, &state.original_urls).to_owned();
                let mut summary = t.to_summary(is_loading);
                summary.url = display;
                if let Some(live_title) = titles
                    .as_ref()
                    .and_then(|m| m.get(&t.target_id))
                    .filter(|s| !s.is_empty())
                {
                    summary.title = live_title.clone();
                }
                summary
            })
            .collect()
    }

    fn tab_count(&self) -> usize {
        self.lock_state().tabs.len()
    }

    fn pinned_count(&self) -> usize {
        0
    }

    fn active_index(&self) -> Option<usize> {
        let state = self.lock_state();
        let active = state.active?;
        state.tabs.iter().position(|t| t.id == active)
    }

    fn can_go_back(&self) -> bool {
        let session_id = {
            let state = self.lock_state();
            state.active_tab().map(|t| t.session_id.clone())
        };
        let Some(sess) = session_id else { return false };
        self.nav_history(&sess)
            .map(|h| h.can_go_back())
            .unwrap_or(false)
    }

    fn can_go_forward(&self) -> bool {
        let session_id = {
            let state = self.lock_state();
            state.active_tab().map(|t| t.session_id.clone())
        };
        let Some(sess) = session_id else { return false };
        self.nav_history(&sess)
            .map(|h| h.can_go_forward())
            .unwrap_or(false)
    }

    // ── Navigation ───────────────────────────────────────────────────────────

    fn navigate(&self, url: &str) -> Result<(), EngineError> {
        tracing::debug!(url, "firefox-cdp: navigate");

        let (session_id, target_id) = {
            let state = self.lock_state();
            let tab = state.active_tab().ok_or(EngineError::NoActiveTab)?;
            (tab.session_id.clone(), tab.target_id.clone())
        };

        // Invalidate nav-history cache — the current position will change.
        self.invalidate_nav_history(&session_id);

        // Optimistically update state; real URL arrives via Page.frameNavigated.
        {
            let mut state = self.lock_state();
            if let Some(id) = state.active
                && let Some(tab) = state.tabs.iter_mut().find(|t| t.id == id)
            {
                tab.url = url.to_owned();
                tab.title = url.to_owned();
            }
            state.original_urls.remove(&target_id);
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
            .recv_timeout(Duration::from_secs(
                crate::worker::CDP_RESPONSE_TIMEOUT_SECS,
            ))
            .map_err(|_| EngineError::Other("navigate timed out".into()))
            .and_then(|r| r.map(|_| ()).map_err(EngineError::from))
    }

    fn active_tab_live_url(&self) -> String {
        let state = self.lock_state();
        state
            .active_tab()
            .map(|t| display_url_for(t, &state.original_urls).to_owned())
            .unwrap_or_default()
    }

    fn pump_address_changes(&self) -> bool {
        let updates: Vec<(String, String, String)> = {
            match self.url_update_sink.lock() {
                Ok(mut sink) => sink.drain(..).collect(),
                Err(_) => return false,
            }
        };

        if updates.is_empty() {
            return false;
        }

        let mut changed = false;
        let mut navigated_sessions: Vec<String> = Vec::new();
        {
            let mut state = self.lock_state();
            for (session_id, url, title) in updates {
                let tab_info = state
                    .tabs
                    .iter()
                    .find(|t| t.session_id == session_id)
                    .map(|t| (t.target_id.clone(), t.url.clone()));

                if let Some((target_id, old_url)) = tab_info {
                    let has_original = state.original_urls.contains_key(&target_id);
                    if !has_original && old_url != url {
                        if let Some(t) = state.tabs.iter_mut().find(|t| t.target_id == target_id) {
                            t.url = url;
                            if !title.is_empty() {
                                t.title = title;
                            }
                        }
                        navigated_sessions.push(session_id);
                        changed = true;
                    }
                }
            }
        }
        // Invalidate nav-history cache for every session that just navigated.
        for sess in navigated_sessions {
            self.invalidate_nav_history(&sess);
        }
        changed
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
        tracing::debug!(
            scale,
            "firefox-cdp: set_device_scale (scale stored, not forwarded to CDP)"
        );
    }

    fn set_frame_rate(&self, hz: u32) {
        use std::sync::atomic::Ordering;
        self.osr_view.frame_rate_hz.store(hz, Ordering::Relaxed);
        // Phase B: poll rate is hard-coded at 4 fps in the worker.
        // Phase C will wire this to the OSR_POLL_INTERVAL_MS via an atomic.
        tracing::debug!(
            hz,
            "firefox-cdp: set_frame_rate (hard-coded 4 fps in Phase B)"
        );
    }

    fn notify_screen_info_changed(&self) {
        // No-op.
    }

    fn osr_resize(&self, width: u32, height: u32) {
        tracing::debug!(width, height, "firefox-cdp: osr_resize");
        let session_id = self
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .active_tab()
            .map(|t| t.session_id.clone());
        if let Some(sess) = session_id {
            let _ = self.cmd_tx.try_send(Command::Resize {
                session_id: sess,
                width: width.max(1),
                height: height.max(1),
            });
        }
        if let Ok(mut frame) = self.osr_frame.lock() {
            frame.needs_fresh = true;
        }
    }

    // ── Input ────────────────────────────────────────────────────────────────

    fn osr_key_event(&self, event: NeutralKeyEvent) {
        let session_id = self
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .active_tab()
            .map(|t| t.session_id.clone());
        let Some(session_id) = session_id else { return };

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
            .unwrap_or_else(|p| p.into_inner())
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
            .unwrap_or_else(|p| p.into_inner())
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
            .unwrap_or_else(|p| p.into_inner())
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
        // Poll loop handles repaint; no explicit trigger needed.
    }

    fn osr_sleep(&self, _sleep: bool) {
        // Firefox CDP poll loop continues regardless; Phase C will pause it on sleep.
    }

    fn osr_invalidate_view(&self) {
        // No-op; poll loop handles invalidation.
    }

    fn set_osr_wake(&self, wake: Arc<dyn Fn() + Send + Sync>) {
        self.osr_view.set_wake(wake);
    }

    // ── Stubs (Phase C) ──────────────────────────────────────────────────────

    fn start_find(&self, query: &str, forward: bool) {
        // Firefox CDP has no Page.find — use window.find via Runtime.evaluate.
        // Backwards search is the second arg's logical inverse. JS-quote the
        // query to escape backslashes + single quotes; nothing else is needed
        // for this string literal context.

        // Store for FindNext / FindPrev dispatch.
        if let Ok(mut g) = self.find_query.lock() {
            *g = Some(query.to_owned());
        }

        let escaped = query.replace('\\', r"\\").replace('\'', r"\'");
        let js = format!(
            "window.find('{escaped}', false /* caseSensitive */, {} /* backwards */, true /* wrapAround */, false /* wholeWord */, true /* searchInFrames */, false /* showDialog */)",
            !forward,
        );
        let Some(session_id) = ({
            let state = self.lock_state();
            state.active.and_then(|id| {
                state
                    .tabs
                    .iter()
                    .find(|t| t.id == id)
                    .map(|t| t.session_id.clone())
            })
        }) else {
            return;
        };
        let _ = self.session_cmd(
            &session_id,
            "Runtime.evaluate",
            serde_json::json!({
                "expression": js,
                "returnByValue": true,
            }),
        );
    }

    fn stop_find(&self) {
        // Clear the stored query so FindNext / FindPrev become no-ops.
        if let Ok(mut g) = self.find_query.lock() {
            *g = None;
        }

        // window.find leaves a selection — clearing window.getSelection()
        // matches Chrome's Page.stopFind { action: "clearSelection" } behaviour.
        let Some(session_id) = ({
            let state = self.lock_state();
            state.active.and_then(|id| {
                state
                    .tabs
                    .iter()
                    .find(|t| t.id == id)
                    .map(|t| t.session_id.clone())
            })
        }) else {
            return;
        };
        let _ = self.session_cmd(
            &session_id,
            "Runtime.evaluate",
            serde_json::json!({
                "expression": "window.getSelection().removeAllRanges()",
                "returnByValue": true,
            }),
        );
    }

    fn active_zoom_level(&self) -> f64 {
        let state = self.lock_state();
        state
            .active
            .and_then(|id| state.tabs.iter().find(|t| t.id == id))
            .map(|t| t.zoom)
            .unwrap_or(1.0)
    }

    fn zoom_in(&self) {
        let next = (self.active_zoom_level() + 0.1).min(5.0);
        let _ = self.apply_zoom(next);
    }

    fn zoom_out(&self) {
        let next = (self.active_zoom_level() - 0.1).max(0.25);
        let _ = self.apply_zoom(next);
    }

    fn zoom_reset(&self) {
        let _ = self.apply_zoom(1.0);
    }

    fn any_audio_active(&self) -> bool {
        false
    }

    fn any_video_active(&self) -> bool {
        false
    }

    fn popup_queue(&self) -> PopupQueue {
        Arc::clone(&self.popup_queue)
    }

    fn popup_create_sink(&self) -> PopupCreateSink {
        Arc::clone(&self.popup_create_sink)
    }

    fn popup_close_sink(&self) -> PopupCloseSink {
        Arc::clone(&self.popup_close_sink)
    }

    fn popup_resize(&self, _browser_id: i32, _width: u32, _height: u32) {
        tracing::debug!("firefox-cdp: popup_resize not implemented");
    }

    fn popup_close(&self, _browser_id: i32) {
        tracing::debug!("firefox-cdp: popup_close not implemented");
    }

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
    // Mirrors the blink-cdp implementation: history/stop via CDP Page commands,
    // scroll via Runtime.evaluate JS injection, zoom via the existing zoom_*
    // helpers, and FindNext/FindPrev via window.find re-invocation.

    fn dispatch(&self, action: &buffr_modal::PageAction) {
        use buffr_modal::PageAction as A;

        const STEP_PX: i64 = 40;

        // Fire-and-forget Runtime.evaluate on the active session.
        let eval = |expr: String| {
            let session_id = {
                let state = self.lock_state();
                state.active_tab().map(|t| t.session_id.clone())
            };
            if let Some(sess) = session_id {
                let _ = self.session_cmd(
                    &sess,
                    "Runtime.evaluate",
                    serde_json::json!({ "expression": expr }),
                );
            }
        };

        match action {
            // ── Find ─────────────────────────────────────────────────────────
            A::FindNext => {
                let query = self.find_query.lock().ok().and_then(|g| g.clone());
                if let Some(q) = query {
                    tracing::debug!(query = %q, "firefox-cdp: dispatch FindNext");
                    let escaped = q.replace('\\', r"\\").replace('\'', r"\'");
                    let js =
                        format!("window.find('{escaped}', false, false, true, false, true, false)");
                    eval(js);
                } else {
                    tracing::debug!("firefox-cdp: FindNext — no active find query");
                }
            }
            A::FindPrev => {
                let query = self.find_query.lock().ok().and_then(|g| g.clone());
                if let Some(q) = query {
                    tracing::debug!(query = %q, "firefox-cdp: dispatch FindPrev");
                    let escaped = q.replace('\\', r"\\").replace('\'', r"\'");
                    let js =
                        format!("window.find('{escaped}', false, true, true, false, true, false)");
                    eval(js);
                } else {
                    tracing::debug!("firefox-cdp: FindPrev — no active find query");
                }
            }

            // ── History / reload / stop ───────────────────────────────────────
            A::HistoryBack => {
                tracing::debug!("firefox-cdp: dispatch HistoryBack");
                eval("window.history.back();".to_owned());
            }
            A::HistoryForward => {
                tracing::debug!("firefox-cdp: dispatch HistoryForward");
                eval("window.history.forward();".to_owned());
            }
            A::Reload => {
                tracing::debug!("firefox-cdp: dispatch Reload");
                let session_id = {
                    let state = self.lock_state();
                    state.active_tab().map(|t| t.session_id.clone())
                };
                if let Some(sess) = session_id {
                    let _ = self.session_cmd(
                        &sess,
                        "Page.reload",
                        serde_json::json!({ "ignoreCache": false }),
                    );
                }
            }
            A::ReloadHard => {
                tracing::debug!("firefox-cdp: dispatch ReloadHard");
                let session_id = {
                    let state = self.lock_state();
                    state.active_tab().map(|t| t.session_id.clone())
                };
                if let Some(sess) = session_id {
                    let _ = self.session_cmd(
                        &sess,
                        "Page.reload",
                        serde_json::json!({ "ignoreCache": true }),
                    );
                }
            }
            A::StopLoading => {
                tracing::debug!("firefox-cdp: dispatch StopLoading");
                let session_id = {
                    let state = self.lock_state();
                    state.active_tab().map(|t| t.session_id.clone())
                };
                if let Some(sess) = session_id {
                    let _ = self.session_cmd(&sess, "Page.stopLoading", serde_json::json!({}));
                }
            }

            // ── Scroll ────────────────────────────────────────────────────────
            A::ScrollUp(n) => {
                let dy = -(STEP_PX * (*n as i64));
                tracing::debug!(n, dy, "firefox-cdp: dispatch ScrollUp");
                eval(format!("window.scrollBy(0, {dy});"));
            }
            A::ScrollDown(n) => {
                let dy = STEP_PX * (*n as i64);
                tracing::debug!(n, dy, "firefox-cdp: dispatch ScrollDown");
                eval(format!("window.scrollBy(0, {dy});"));
            }
            A::ScrollLeft(n) => {
                let dx = -(STEP_PX * (*n as i64));
                tracing::debug!(n, dx, "firefox-cdp: dispatch ScrollLeft");
                eval(format!("window.scrollBy({dx}, 0);"));
            }
            A::ScrollRight(n) => {
                let dx = STEP_PX * (*n as i64);
                tracing::debug!(n, dx, "firefox-cdp: dispatch ScrollRight");
                eval(format!("window.scrollBy({dx}, 0);"));
            }
            A::ScrollPageDown | A::ScrollFullPageDown => {
                tracing::debug!("firefox-cdp: dispatch ScrollPageDown");
                eval("window.scrollBy(0, window.innerHeight * 0.9);".to_owned());
            }
            A::ScrollPageUp | A::ScrollFullPageUp => {
                tracing::debug!("firefox-cdp: dispatch ScrollPageUp");
                eval("window.scrollBy(0, -window.innerHeight * 0.9);".to_owned());
            }
            A::ScrollHalfPageDown => {
                tracing::debug!("firefox-cdp: dispatch ScrollHalfPageDown");
                eval("window.scrollBy(0, window.innerHeight * 0.5);".to_owned());
            }
            A::ScrollHalfPageUp => {
                tracing::debug!("firefox-cdp: dispatch ScrollHalfPageUp");
                eval("window.scrollBy(0, -window.innerHeight * 0.5);".to_owned());
            }
            A::ScrollTop => {
                tracing::debug!("firefox-cdp: dispatch ScrollTop");
                eval("window.scrollTo(0, 0);".to_owned());
            }
            A::ScrollBottom => {
                tracing::debug!("firefox-cdp: dispatch ScrollBottom");
                eval("window.scrollTo(0, document.body.scrollHeight);".to_owned());
            }

            // ── Zoom ──────────────────────────────────────────────────────────
            A::ZoomIn => self.zoom_in(),
            A::ZoomOut => self.zoom_out(),
            A::ZoomReset => self.zoom_reset(),

            other => {
                tracing::debug!(
                    action = ?other,
                    "firefox-cdp: dispatch — action not handled by this backend (no-op)"
                );
            }
        }
    }

    // ── IME composition (Phase 8d, #86) ──────────────────────────────────────
    //
    // Firefox CDP exposes `Input.imeSetComposition` and `Input.insertText`.
    //
    // `ime_set_composition` → `Input.imeSetComposition` with selectionStart /
    //   selectionEnd / replacementStart / replacementEnd params. When `cursor`
    //   is `Some((s, e))` the selection range [s, e] within the composition
    //   string is highlighted.  When `None` we use the text length as both
    //   endpoints (cursor at end, no range selected).
    //
    // `ime_commit` → `Input.insertText { text }`.  Sends the committed text
    //   directly to the focused element.  No prior `Input.imeSetComposition`
    //   clear is required — Firefox CDP handles the transition internally.
    //
    // `ime_cancel` → `Input.imeSetComposition { text: "" }`.  An empty
    //   composition string clears the in-progress composition preedit.
    //
    // Source: Firefox CDP Remote Agent implements Input.imeSetComposition and
    //   Input.insertText in `dom/chrome/test/firefox-cdp/input.js` and the
    //   corresponding actor at `remote/components/geckoviewactor/InputMethodActor.jsm`.

    fn ime_set_composition(&self, text: &str, cursor: Option<(usize, usize)>) {
        let (sel_start, sel_end) = cursor.unwrap_or_else(|| {
            let len = text.len();
            (len, len)
        });
        tracing::debug!(text, sel_start, sel_end, "firefox-cdp: ime_set_composition");
        let session_id = {
            let state = self.lock_state();
            state.active_tab().map(|t| t.session_id.clone())
        };
        if let Some(sess) = session_id {
            let _ = self.session_cmd(
                &sess,
                "Input.imeSetComposition",
                serde_json::json!({
                    "text": text,
                    "selectionStart": sel_start,
                    "selectionEnd": sel_end,
                    "replacementStart": 0,
                    "replacementEnd": 0,
                }),
            );
        }
    }

    fn ime_commit(&self, text: &str) {
        tracing::debug!(text, "firefox-cdp: ime_commit");
        let session_id = {
            let state = self.lock_state();
            state.active_tab().map(|t| t.session_id.clone())
        };
        if let Some(sess) = session_id {
            let _ = self.session_cmd(
                &sess,
                "Input.insertText",
                serde_json::json!({ "text": text }),
            );
        }
    }

    fn ime_cancel(&self) {
        tracing::debug!(
            "firefox-cdp: ime_cancel — clearing composition via empty imeSetComposition"
        );
        let session_id = {
            let state = self.lock_state();
            state.active_tab().map(|t| t.session_id.clone())
        };
        if let Some(sess) = session_id {
            let _ = self.session_cmd(
                &sess,
                "Input.imeSetComposition",
                serde_json::json!({
                    "text": "",
                    "selectionStart": 0,
                    "selectionEnd": 0,
                    "replacementStart": 0,
                    "replacementEnd": 0,
                }),
            );
        }
    }

    // ── JS execution (Phase 6c, #95) ─────────────────────────────────────────
    //
    // Firefox CDP supports `Runtime.evaluate` on the active session, matching
    // the blink-cdp pattern almost verbatim.  `run_js` delegates to
    // `run_main_frame_js` (the `_url` origin parameter is ignored by the
    // Gecko CDP agent, which derives the script origin from the active frame).

    fn run_js(&self, code: &str) -> Result<(), buffr_engine::EngineError> {
        self.run_main_frame_js(code, "")
    }

    fn run_main_frame_js(&self, code: &str, _url: &str) -> Result<(), buffr_engine::EngineError> {
        let session_id = {
            let state = self.lock_state();
            state
                .active_tab()
                .map(|t| t.session_id.clone())
                .ok_or(buffr_engine::EngineError::NoActiveTab)?
        };
        self.session_cmd(
            &session_id,
            "Runtime.evaluate",
            serde_json::json!({ "expression": code }),
        )
        .map(|_| ())
        .map_err(|e| buffr_engine::EngineError::Other(e.to_string()))
    }

    // ── Downloads (Phase 6c, #95) ────────────────────────────────────────────

    fn start_download(&self, url: &str) {
        tracing::debug!("firefox-cdp: start_download url={url}");
        let url_json = serde_json::to_string(url).unwrap_or_else(|_| "\"\"".into());
        let js = format!(
            "(() => {{ const a = document.createElement('a'); \
             a.href = {url_json}; a.download = ''; \
             document.body.appendChild(a); a.click(); a.remove(); }})();"
        );
        let _ = self.run_js(&js);
    }
}
