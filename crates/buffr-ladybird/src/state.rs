//! [`LadybirdState`] — worker-thread state for the Ladybird engine.
//!
//! # Why a worker thread?
//!
//! `cxx::UniquePtr<WebContent>` is `Send` (cxx marks opaque C++ types as `Send`
//! when the bridge is declared `unsafe extern "C++"`), but the real Ladybird
//! WebContentServer uses thread-affine IPC sockets.  We follow the Blitz pattern
//! and own all `WebContent` instances on a dedicated thread, accessed via `mpsc`.
//! This also keeps `LadybirdEngine` (`&self`) naturally `Send + Sync` without
//! any `unsafe` on the Rust side.
//!
//! # OSR pipeline
//!
//! After every state mutation the worker calls `paint()`, which:
//!  1. Locks `SharedOsrFrame`.
//!  2. Ensures the pixel buffer is sized correctly (`width * height * 4`).
//!  3. Calls `webcontent_read_pixels` on the active tab (Phase B: fills dark grey).
//!  4. Bumps `generation` and clears `needs_fresh`.
//!  5. Fires the optional wake callback.
//!
//! Phase C replaces the `read_pixels` call with real LibGfx surface readback.

use std::sync::atomic::Ordering;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use buffr_engine::{SharedOsrFrame, SharedOsrViewState, TabId, TabSummary};
use cxx::UniquePtr;

use crate::error::LadybirdError;
use crate::ffi::bridge::{
    WebContent, webcontent_any_audio_active, webcontent_any_video_active, webcontent_can_go_back,
    webcontent_can_go_forward, webcontent_edit_attach, webcontent_edit_cycle,
    webcontent_edit_detach, webcontent_edit_focus, webcontent_eval_js,
    webcontent_eval_main_frame_js, webcontent_go_back, webcontent_go_forward,
    webcontent_ime_cancel, webcontent_ime_commit, webcontent_ime_set_composition,
    webcontent_is_loading, webcontent_navigate, webcontent_new, webcontent_read_pixels,
    webcontent_reload, webcontent_resize, webcontent_send_key, webcontent_send_mouse_button,
    webcontent_send_mouse_move, webcontent_send_scroll, webcontent_set_sleep, webcontent_set_zoom,
    webcontent_start_download, webcontent_stop, webcontent_title, webcontent_url, webcontent_zoom,
};

// ── Tab ───────────────────────────────────────────────────────────────────────

pub(crate) struct LadybirdTab {
    pub id: TabId,
    wc: UniquePtr<WebContent>,
}

impl LadybirdTab {
    fn new(id: TabId, url: &str, width: u32, height: u32) -> Self {
        let wc = webcontent_new(url, width, height);
        LadybirdTab { id, wc }
    }

    pub(crate) fn url(&self) -> String {
        webcontent_url(&self.wc)
    }

    pub(crate) fn title(&self) -> String {
        webcontent_title(&self.wc)
    }

    pub(crate) fn can_go_back(&self) -> bool {
        webcontent_can_go_back(&self.wc)
    }

    pub(crate) fn can_go_forward(&self) -> bool {
        webcontent_can_go_forward(&self.wc)
    }

    pub(crate) fn navigate(&mut self, url: &str) {
        webcontent_navigate(self.wc.pin_mut(), url);
    }

    pub(crate) fn reload(&mut self) {
        webcontent_reload(self.wc.pin_mut());
    }

    pub(crate) fn stop(&mut self) {
        webcontent_stop(self.wc.pin_mut());
    }

    pub(crate) fn go_back(&mut self) {
        webcontent_go_back(self.wc.pin_mut());
    }

    pub(crate) fn go_forward(&mut self) {
        webcontent_go_forward(self.wc.pin_mut());
    }

    pub(crate) fn resize(&mut self, width: u32, height: u32) {
        webcontent_resize(self.wc.pin_mut(), width, height);
    }

    pub(crate) fn read_pixels_into(&self, buf: &mut Vec<u8>, width: u32, height: u32) {
        let len = (width as usize) * (height as usize) * 4;
        if buf.len() != len {
            buf.resize(len, 0);
        }
        webcontent_read_pixels(&self.wc, buf.as_mut_slice());
    }

    /// Set the page zoom level.
    ///
    /// Phase B: forwarded to the C++ stub which stores it in `m_zoom`.
    /// Phase C: wires to LibWeb `Page::set_zoom_level` via the IPC shim.
    pub(crate) fn set_zoom(&mut self, zoom: f64) {
        webcontent_set_zoom(self.wc.pin_mut(), zoom);
    }

    /// Read the current page zoom level from the C++ stub.
    pub(crate) fn zoom(&self) -> f64 {
        webcontent_zoom(&self.wc)
    }

    /// Update IME preedit string.
    ///
    /// Phase B: stored in `m_ime_composition` (no render effect).
    /// Phase C: IPC SetComposition to WebContentServer.
    pub(crate) fn ime_set_composition(&mut self, text: &str, sel_start: u32, sel_end: u32) {
        webcontent_ime_set_composition(self.wc.pin_mut(), text, sel_start, sel_end);
    }

    /// Commit composed text into the focused element.
    ///
    /// Phase B: clears `m_ime_composition` (no render effect).
    /// Phase C: IPC CommitComposition to WebContentServer.
    pub(crate) fn ime_commit(&mut self, text: &str) {
        webcontent_ime_commit(self.wc.pin_mut(), text);
    }

    /// Cancel the current IME composition.
    ///
    /// Phase B: clears `m_ime_composition` (no render effect).
    /// Phase C: IPC CancelComposition to WebContentServer.
    pub(crate) fn ime_cancel(&mut self) {
        webcontent_ime_cancel(self.wc.pin_mut());
    }

    /// Evaluate `code` in the JS context (fire-and-forget).
    ///
    /// Phase B stub: logs and no-ops.
    /// Phase C: IPC execute_script to WebContentServer.
    /// TODO(phase-c): wire to real LibJS dispatch.
    pub(crate) fn eval_js(&mut self, code: &str) {
        webcontent_eval_js(self.wc.pin_mut(), code);
    }

    /// Evaluate `code` attributed to `url` (fire-and-forget).
    ///
    /// Phase B stub: logs and no-ops.
    /// Phase C: IPC execute_script with origin URL to WebContentServer.
    /// TODO(phase-c): wire to real LibJS dispatch with script URL.
    pub(crate) fn eval_main_frame_js(&mut self, code: &str, url: &str) {
        webcontent_eval_main_frame_js(self.wc.pin_mut(), code, url);
    }

    // ── Edit IPC helpers ──────────────────────────────────────────────────────

    /// Attach the edit-active CSS class to `field_id`.
    ///
    /// Phase B stub: no-ops.
    /// Phase C: IPC `__buffrEditAttach` to LibWeb JS.
    pub(crate) fn edit_attach(&mut self, field_id: &str) {
        cxx::let_cxx_string!(cxx_field_id = field_id);
        webcontent_edit_attach(self.wc.pin_mut(), &cxx_field_id);
    }

    /// Cycle focus to the next/previous visible input.
    ///
    /// Phase B stub: no-ops.
    /// Phase C: IPC `__buffrEditCycle` to LibWeb JS.
    pub(crate) fn edit_cycle(&mut self, forward: bool) {
        webcontent_edit_cycle(self.wc.pin_mut(), forward);
    }

    /// Detach the edit-active CSS class from `field_id`.
    ///
    /// Phase B stub: no-ops.
    /// Phase C: IPC `__buffrEditDetach` to LibWeb JS.
    pub(crate) fn edit_detach(&mut self, field_id: &str) {
        cxx::let_cxx_string!(cxx_field_id = field_id);
        webcontent_edit_detach(self.wc.pin_mut(), &cxx_field_id);
    }

    /// Re-focus the element identified by `field_id`.
    ///
    /// Phase B stub: no-ops.
    /// Phase C: IPC `__buffrEditFocus` to LibWeb JS.
    pub(crate) fn edit_focus(&mut self, field_id: &str) {
        cxx::let_cxx_string!(cxx_field_id = field_id);
        webcontent_edit_focus(self.wc.pin_mut(), &cxx_field_id);
    }

    // ── Audio / video activity ────────────────────────────────────────────────

    /// Whether this WebContent has an active audio stream.
    ///
    /// Phase B stub: always false.
    /// Phase C: query real Ladybird media-activity state.
    pub(crate) fn any_audio_active(&self) -> bool {
        webcontent_any_audio_active(&self.wc)
    }

    /// Whether this WebContent has an active video stream.
    ///
    /// Phase B stub: always false.
    /// Phase C: query real Ladybird media-activity state.
    pub(crate) fn any_video_active(&self) -> bool {
        webcontent_any_video_active(&self.wc)
    }

    // ── Sleep / wake ──────────────────────────────────────────────────────────

    /// Put this WebContent to sleep or wake it.
    ///
    /// Phase B stub: no-ops.
    /// Phase C: Ladybird WebContent visibility/suspend IPC.
    pub(crate) fn set_sleep(&mut self, sleep: bool) {
        webcontent_set_sleep(self.wc.pin_mut(), sleep);
    }

    // ── Downloads ─────────────────────────────────────────────────────────────

    /// Trigger a managed browser download for `url`.
    ///
    /// Phase B stub: no-ops.
    /// Phase C: trigger the Ladybird download manager.
    /// TODO(phase-c): wire to real Ladybird download infra.
    pub(crate) fn start_download(&mut self, url: &str) {
        cxx::let_cxx_string!(cxx_url = url);
        webcontent_start_download(self.wc.pin_mut(), &cxx_url);
    }

    // ── Loading state ─────────────────────────────────────────────────────────

    /// Whether this WebContent is currently loading a document.
    ///
    /// Phase B stub: always false.
    /// Phase C: reflect real Ladybird page-load state.
    pub(crate) fn is_loading(&self) -> bool {
        webcontent_is_loading(&self.wc)
    }

    #[allow(dead_code)]
    pub(crate) fn to_summary(&self) -> TabSummary {
        TabSummary {
            id: self.id,
            browser_id: 0,
            title: self.title(),
            url: self.url(),
            progress: 1.0,
            is_loading: false,
            pinned: false,
            private: false,
        }
    }
}

// ── Commands ──────────────────────────────────────────────────────────────────

#[allow(dead_code)]
pub(crate) enum Command {
    OpenTab {
        url: String,
        reply: mpsc::SyncSender<Result<TabId, LadybirdError>>,
    },
    /// Open a tab and insert it at `insert_idx` (clamped to len).
    OpenTabAt {
        url: String,
        insert_idx: usize,
        reply: mpsc::SyncSender<Result<TabId, LadybirdError>>,
    },
    CloseTab {
        id: TabId,
        reply: mpsc::SyncSender<Result<bool, LadybirdError>>,
    },
    SelectTab {
        id: TabId,
    },
    CycleTab {
        forward: bool,
    },
    /// Move the tab at `from` to `to` (both clamped to current len).
    MoveTab {
        from: usize,
        to: usize,
    },
    Navigate {
        url: String,
        reply: mpsc::SyncSender<Result<(), LadybirdError>>,
    },
    GoBack,
    GoForward,
    Reload,
    Stop,
    Resize {
        width: u32,
        height: u32,
    },
    ForcePaint,
    QueryTabs {
        reply: mpsc::SyncSender<TabsSnapshot>,
    },
    QueryCanGoBack {
        reply: mpsc::SyncSender<bool>,
    },
    QueryCanGoForward {
        reply: mpsc::SyncSender<bool>,
    },
    SendKey {
        key_code: u32,
        is_press: bool,
        modifiers: u32,
    },
    SendMouseMove {
        x: i32,
        y: i32,
    },
    SendMouseButton {
        button: u32,
        x: i32,
        y: i32,
        is_press: bool,
    },
    SendScroll {
        x: i32,
        y: i32,
        dx: f64,
        dy: f64,
    },
    /// Set the active tab's page zoom level. Clamped to [0.25, 5.0].
    ///
    /// Forwarded to `webcontent_set_zoom` in the C++ shim.
    /// Phase C: maps to LibWeb `Page::set_zoom_level`.
    SetZoom(f64),
    /// Query the active tab's current zoom level.
    ///
    /// Reads back from `webcontent_zoom` in the C++ shim (returns `m_zoom`).
    QueryActiveZoom {
        reply: mpsc::SyncSender<f64>,
    },
    /// Update the active tab's IME preedit string.
    ///
    /// Phase B: forwarded to `webcontent_ime_set_composition` in the C++ shim,
    ///   which stores the text in `m_ime_composition` (no render effect).
    /// Phase C: IPC message to WebContentServer's IME handler.
    ImeSetComposition {
        text: String,
        sel_start: u32,
        sel_end: u32,
    },
    /// Commit the composed text into the active tab's focused element.
    ///
    /// Phase B: forwarded to `webcontent_ime_commit` (clears `m_ime_composition`).
    /// Phase C: IPC CommitComposition message.
    ImeCommit {
        text: String,
    },
    /// Cancel the current IME composition.
    ///
    /// Phase B: forwarded to `webcontent_ime_cancel` (clears `m_ime_composition`).
    /// Phase C: IPC CancelComposition message.
    ImeCancel,
    /// Evaluate `code` in the active tab's JS context (fire-and-forget).
    ///
    /// Phase B: forwarded to `webcontent_eval_js` in the C++ shim, which
    ///   logs and no-ops (LibJS not linked).
    /// Phase C: IPC execute_script message to the Ladybird WebContentServer.
    /// TODO(phase-c): wire to real LibJS dispatch once the IPC surface is stable.
    EvalJs {
        code: String,
    },
    /// Evaluate `code` attributed to `url` in the active tab's JS context.
    ///
    /// Phase B: forwarded to `webcontent_eval_main_frame_js` (logs, no-ops).
    /// Phase C: same as EvalJs but also sets the script origin URL.
    /// TODO(phase-c): wire to real LibJS dispatch with script URL.
    EvalMainFrameJs {
        code: String,
        url: String,
    },
    /// Attach the edit-active CSS class to `field_id` on the active tab.
    ///
    /// Phase B: forwarded to `webcontent_edit_attach` (no-ops).
    /// Phase C: IPC `__buffrEditAttach` to LibWeb JS.
    EditAttach {
        field_id: String,
    },
    /// Cycle focus to the next/previous visible input on the active tab.
    ///
    /// Phase B: forwarded to `webcontent_edit_cycle` (no-ops).
    /// Phase C: IPC `__buffrEditCycle` to LibWeb JS.
    EditCycle {
        forward: bool,
    },
    /// Detach the edit-active CSS class from `field_id` on the active tab.
    ///
    /// Phase B: forwarded to `webcontent_edit_detach` (no-ops).
    /// Phase C: IPC `__buffrEditDetach` to LibWeb JS.
    EditDetach {
        field_id: String,
    },
    /// Re-focus the element identified by `field_id` on the active tab.
    ///
    /// Phase B: forwarded to `webcontent_edit_focus` (no-ops).
    /// Phase C: IPC `__buffrEditFocus` to LibWeb JS.
    EditFocus {
        field_id: String,
    },
    /// Query whether any tab has an active audio stream.
    ///
    /// Phase B: always returns false (stub).
    /// Phase C: query real Ladybird media-activity state.
    QueryAnyAudioActive {
        reply: mpsc::SyncSender<bool>,
    },
    /// Query whether any tab has an active video stream.
    ///
    /// Phase B: always returns false (stub).
    /// Phase C: query real Ladybird media-activity state.
    QueryAnyVideoActive {
        reply: mpsc::SyncSender<bool>,
    },
    /// Put the active tab to sleep or wake it.
    ///
    /// Phase B: forwarded to `webcontent_set_sleep` (no-ops).
    /// Phase C: Ladybird WebContent visibility/suspend IPC.
    SetSleep {
        sleep: bool,
    },
    /// Trigger a managed browser download for `url` on the active tab.
    ///
    /// Phase B: forwarded to `webcontent_start_download` (no-ops).
    /// Phase C: trigger the Ladybird download manager.
    /// TODO(phase-c): wire to real Ladybird download infra.
    StartDownload {
        url: String,
    },
    /// Query whether the active tab is currently loading a document.
    ///
    /// Phase B: always returns false (stub).
    /// Phase C: reflect real Ladybird page-load state.
    QueryIsLoading {
        reply: mpsc::SyncSender<bool>,
    },
    /// Drain pending favicon updates from all tabs.
    ///
    /// Phase B: always returns an empty Vec — favicon push notifications are
    /// a C++-to-Rust callback that Phase C will wire via a shared queue. For
    /// now the worker has no favicon state to drain.
    ///
    /// Phase C: wire to a `SharedFaviconQueue` populated by an
    ///   `on_favicon_change` Ladybird callback.
    DrainFavicons {
        reply: mpsc::SyncSender<Vec<buffr_engine::FaviconUpdate>>,
    },
    Shutdown,
}

// ── Tab cache (engine-side) ───────────────────────────────────────────────────

/// Cached snapshot of the worker's tab list, kept on the engine side.
#[derive(Default)]
pub(crate) struct TabCache {
    pub summaries: Vec<buffr_engine::TabSummary>,
    pub active_idx: Option<usize>,
}

// ── Tab snapshot (cross-thread) ───────────────────────────────────────────────

pub(crate) struct TabRecord {
    pub id: TabId,
    pub url: String,
    pub title: String,
    #[allow(dead_code)]
    pub can_go_back: bool,
    #[allow(dead_code)]
    pub can_go_forward: bool,
}

pub(crate) struct TabsSnapshot {
    pub tabs: Vec<TabRecord>,
    pub active: Option<TabId>,
}

impl TabRecord {
    pub(crate) fn to_summary(&self) -> TabSummary {
        TabSummary {
            id: self.id,
            browser_id: 0,
            title: self.title.clone(),
            url: self.url.clone(),
            progress: 1.0,
            is_loading: false,
            pinned: false,
            private: false,
        }
    }
}

// ── Worker handle ─────────────────────────────────────────────────────────────

#[derive(Clone)]
pub(crate) struct WorkerHandle {
    tx: mpsc::SyncSender<Command>,
}

impl WorkerHandle {
    pub(crate) fn send(&self, cmd: Command) {
        let _ = self.tx.send(cmd);
    }

    pub(crate) fn call<T: Send + 'static>(
        &self,
        build: impl FnOnce(mpsc::SyncSender<T>) -> Command,
    ) -> Result<T, LadybirdError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.send(build(reply_tx));
        reply_rx
            .recv_timeout(Duration::from_secs(30))
            .map_err(|_| LadybirdError::Ffi("worker timeout".into()))
    }
}

// ── Worker ────────────────────────────────────────────────────────────────────

struct Worker {
    tabs: Vec<LadybirdTab>,
    active_idx: Option<usize>,
    next_id: u64,
    frame: SharedOsrFrame,
    view: SharedOsrViewState,
    width: u32,
    height: u32,
}

impl Worker {
    fn new(frame: SharedOsrFrame, view: SharedOsrViewState, width: u32, height: u32) -> Self {
        Worker {
            tabs: Vec::new(),
            active_idx: None,
            next_id: 1,
            frame,
            view,
            width,
            height,
        }
    }

    fn next_id(&mut self) -> TabId {
        let id = TabId(self.next_id);
        self.next_id += 1;
        id
    }

    fn idx_by_id(&self, id: TabId) -> Option<usize> {
        self.tabs.iter().position(|t| t.id == id)
    }

    fn open_tab(&mut self, url: &str) -> Result<TabId, LadybirdError> {
        let id = self.next_id();
        tracing::info!("ladybird worker: open_tab {id:?} → {url}");
        let tab = LadybirdTab::new(id, url, self.width, self.height);
        self.tabs.push(tab);
        self.active_idx = Some(self.tabs.len() - 1);
        self.paint();
        Ok(id)
    }

    /// Open a tab at `insert_idx` (appended first, then rotated into position).
    fn open_tab_at(&mut self, url: &str, insert_idx: usize) -> Result<TabId, LadybirdError> {
        let id = self.open_tab(url)?;
        let appended = self.tabs.len() - 1;
        let clamped = insert_idx.min(appended);
        if clamped != appended {
            let tab = self.tabs.remove(appended);
            self.tabs.insert(clamped, tab);
            self.active_idx = Some(clamped);
        }
        self.paint();
        Ok(id)
    }

    /// Move tab at `from` to `to` (clamped). Active index follows by TabId.
    fn move_tab(&mut self, from: usize, to: usize) {
        let len = self.tabs.len();
        if len == 0 || from >= len || from == to {
            return;
        }
        let clamped_to = to.min(len - 1);
        if clamped_to == from {
            return;
        }
        let active_id = self.active_idx.and_then(|i| self.tabs.get(i).map(|t| t.id));
        let tab = self.tabs.remove(from);
        self.tabs.insert(clamped_to, tab);
        if let Some(aid) = active_id {
            self.active_idx = self.tabs.iter().position(|t| t.id == aid);
        }
        self.paint();
    }

    fn close_tab(&mut self, id: TabId) -> Result<bool, LadybirdError> {
        let idx = self.idx_by_id(id).ok_or(LadybirdError::TabNotFound(id))?;
        self.tabs.remove(idx);
        match self.active_idx {
            Some(a) if a == idx => {
                self.active_idx = if self.tabs.is_empty() {
                    None
                } else {
                    Some(idx.saturating_sub(1).min(self.tabs.len() - 1))
                };
            }
            Some(a) if a > idx => {
                self.active_idx = Some(a - 1);
            }
            _ => {}
        }
        self.paint();
        Ok(!self.tabs.is_empty())
    }

    fn select_tab(&mut self, id: TabId) {
        if let Some(idx) = self.idx_by_id(id) {
            self.active_idx = Some(idx);
        }
        self.paint();
    }

    fn cycle_tab(&mut self, forward: bool) {
        let n = self.tabs.len();
        if n == 0 {
            return;
        }
        let cur = self.active_idx.unwrap_or(0);
        self.active_idx = Some(if forward {
            (cur + 1) % n
        } else {
            cur.checked_sub(1).unwrap_or(n - 1)
        });
        self.paint();
    }

    fn navigate(&mut self, url: &str) -> Result<(), LadybirdError> {
        let idx = self.active_idx.ok_or(LadybirdError::NoActiveTab)?;
        self.tabs[idx].navigate(url);
        self.paint();
        Ok(())
    }

    fn go_back(&mut self) {
        if let Some(idx) = self.active_idx {
            self.tabs[idx].go_back();
            self.paint();
        }
    }

    fn go_forward(&mut self) {
        if let Some(idx) = self.active_idx {
            self.tabs[idx].go_forward();
            self.paint();
        }
    }

    fn reload(&mut self) {
        if let Some(idx) = self.active_idx {
            self.tabs[idx].reload();
            self.paint();
        }
    }

    fn stop(&mut self) {
        if let Some(idx) = self.active_idx {
            self.tabs[idx].stop();
        }
    }

    fn resize(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
        self.view.width.store(width, Ordering::Relaxed);
        self.view.height.store(height, Ordering::Relaxed);
        for tab in &mut self.tabs {
            tab.resize(width, height);
        }
        self.paint();
    }

    fn send_key(&mut self, key_code: u32, is_press: bool, modifiers: u32) {
        if let Some(idx) = self.active_idx {
            webcontent_send_key(self.tabs[idx].wc.pin_mut(), key_code, is_press, modifiers);
        }
    }

    fn send_mouse_move(&mut self, x: i32, y: i32) {
        if let Some(idx) = self.active_idx {
            webcontent_send_mouse_move(self.tabs[idx].wc.pin_mut(), x, y);
        }
    }

    fn send_mouse_button(&mut self, button: u32, x: i32, y: i32, is_press: bool) {
        if let Some(idx) = self.active_idx {
            webcontent_send_mouse_button(self.tabs[idx].wc.pin_mut(), button, x, y, is_press);
        }
    }

    fn send_scroll(&mut self, x: i32, y: i32, dx: f64, dy: f64) {
        if let Some(idx) = self.active_idx {
            webcontent_send_scroll(self.tabs[idx].wc.pin_mut(), x, y, dx, dy);
        }
    }

    /// Set the active tab's page zoom level.
    ///
    /// Clamps to [0.25, 5.0] and forwards to the C++ shim's
    /// `webcontent_set_zoom`. Phase C wires this to LibWeb's
    /// `Page::set_zoom_level` via the WebContent IPC shim.
    fn set_zoom(&mut self, level: f64) {
        let clamped = level.clamp(0.25, 5.0);
        if let Some(idx) = self.active_idx {
            self.tabs[idx].set_zoom(clamped);
            tracing::debug!("ladybird worker: set_zoom({clamped:.2})");
        }
    }

    /// Query the active tab's current zoom level.
    ///
    /// Reads back from the C++ shim via `webcontent_zoom`, which returns
    /// `m_zoom` (Phase B: no real rendering effect).
    fn query_active_zoom(&self) -> f64 {
        self.active_idx.map(|i| self.tabs[i].zoom()).unwrap_or(1.0)
    }

    /// Forward an IME preedit update to the active tab.
    fn ime_set_composition(&mut self, text: &str, sel_start: u32, sel_end: u32) {
        if let Some(i) = self.active_idx {
            self.tabs[i].ime_set_composition(text, sel_start, sel_end);
        }
    }

    /// Forward an IME commit to the active tab.
    fn ime_commit(&mut self, text: &str) {
        if let Some(i) = self.active_idx {
            self.tabs[i].ime_commit(text);
        }
    }

    /// Cancel IME composition on the active tab.
    fn ime_cancel(&mut self) {
        if let Some(i) = self.active_idx {
            self.tabs[i].ime_cancel();
        }
    }

    /// Evaluate `code` in the active tab's JS context (fire-and-forget).
    fn eval_js(&mut self, code: &str) {
        if let Some(i) = self.active_idx {
            tracing::debug!("ladybird worker: eval_js ({} bytes)", code.len());
            self.tabs[i].eval_js(code);
        } else {
            tracing::debug!("ladybird worker: eval_js — no active tab, dropping");
        }
    }

    /// Evaluate `code` attributed to `url` in the active tab's JS context.
    fn eval_main_frame_js(&mut self, code: &str, url: &str) {
        if let Some(i) = self.active_idx {
            tracing::debug!(
                "ladybird worker: eval_main_frame_js ({} bytes, url={url})",
                code.len()
            );
            self.tabs[i].eval_main_frame_js(code, url);
        } else {
            tracing::debug!("ladybird worker: eval_main_frame_js — no active tab, dropping");
        }
    }

    // ── Edit IPC helpers ──────────────────────────────────────────────────────

    fn edit_attach(&mut self, field_id: &str) {
        if let Some(i) = self.active_idx {
            tracing::debug!("ladybird worker: edit_attach field_id={field_id}");
            self.tabs[i].edit_attach(field_id);
        } else {
            tracing::debug!("ladybird worker: edit_attach — no active tab, dropping");
        }
    }

    fn edit_cycle(&mut self, forward: bool) {
        if let Some(i) = self.active_idx {
            tracing::debug!("ladybird worker: edit_cycle forward={forward}");
            self.tabs[i].edit_cycle(forward);
        } else {
            tracing::debug!("ladybird worker: edit_cycle — no active tab, dropping");
        }
    }

    fn edit_detach(&mut self, field_id: &str) {
        if let Some(i) = self.active_idx {
            tracing::debug!("ladybird worker: edit_detach field_id={field_id}");
            self.tabs[i].edit_detach(field_id);
        } else {
            tracing::debug!("ladybird worker: edit_detach — no active tab, dropping");
        }
    }

    fn edit_focus(&mut self, field_id: &str) {
        if let Some(i) = self.active_idx {
            tracing::debug!("ladybird worker: edit_focus field_id={field_id}");
            self.tabs[i].edit_focus(field_id);
        } else {
            tracing::debug!("ladybird worker: edit_focus — no active tab, dropping");
        }
    }

    // ── Audio / video activity ────────────────────────────────────────────────

    /// `true` when any tab has an active audio stream.
    fn any_audio_active(&self) -> bool {
        self.tabs.iter().any(|t| t.any_audio_active())
    }

    /// `true` when any tab has an active video stream.
    fn any_video_active(&self) -> bool {
        self.tabs.iter().any(|t| t.any_video_active())
    }

    // ── Sleep / wake ──────────────────────────────────────────────────────────

    fn set_sleep(&mut self, sleep: bool) {
        if let Some(i) = self.active_idx {
            tracing::debug!("ladybird worker: set_sleep({sleep})");
            self.tabs[i].set_sleep(sleep);
        } else {
            tracing::debug!("ladybird worker: set_sleep — no active tab, dropping");
        }
    }

    // ── Downloads ─────────────────────────────────────────────────────────────

    fn start_download(&mut self, url: &str) {
        if let Some(i) = self.active_idx {
            tracing::debug!("ladybird worker: start_download url={url}");
            self.tabs[i].start_download(url);
        } else {
            tracing::debug!("ladybird worker: start_download — no active tab, dropping");
        }
    }

    // ── Loading state ─────────────────────────────────────────────────────────

    fn query_is_loading(&self) -> bool {
        self.active_idx
            .map(|i| self.tabs[i].is_loading())
            .unwrap_or(false)
    }

    fn build_snapshot(&self) -> TabsSnapshot {
        let tabs: Vec<TabRecord> = self
            .tabs
            .iter()
            .map(|t| TabRecord {
                id: t.id,
                url: t.url(),
                title: t.title(),
                can_go_back: t.can_go_back(),
                can_go_forward: t.can_go_forward(),
            })
            .collect();
        let active = self.active_idx.and_then(|i| tabs.get(i).map(|t| t.id));
        TabsSnapshot { tabs, active }
    }

    fn active_can_go_back(&self) -> bool {
        self.active_idx
            .map(|i| self.tabs[i].can_go_back())
            .unwrap_or(false)
    }

    fn active_can_go_forward(&self) -> bool {
        self.active_idx
            .map(|i| self.tabs[i].can_go_forward())
            .unwrap_or(false)
    }

    /// Write BGRA pixels from the active tab into `SharedOsrFrame`.
    ///
    /// Phase B: calls `webcontent_read_pixels` which fills dark-grey (0xFF101010).
    /// Phase C: wires real LibGfx bitmap readback.
    fn paint(&mut self) {
        let w = self.width;
        let h = self.height;
        self.view.width.store(w, Ordering::Relaxed);
        self.view.height.store(h, Ordering::Relaxed);

        let mut tmp = Vec::new();

        if let Some(idx) = self.active_idx {
            self.tabs[idx].read_pixels_into(&mut tmp, w, h);
        } else {
            // No active tab: fill with black.
            tmp.resize((w as usize) * (h as usize) * 4, 0);
        }

        if let Ok(mut guard) = self.frame.lock() {
            if guard.width != w || guard.height != h {
                guard.width = w;
                guard.height = h;
            }
            guard.pixels = tmp;
            guard.generation = guard.generation.wrapping_add(1);
            guard.needs_fresh = false;
        }

        if let Some(wake) = self.view.wake.get() {
            wake();
        }
        tracing::debug!("ladybird worker: painted frame {w}×{h}");
    }

    fn run(mut self, rx: mpsc::Receiver<Command>) {
        tracing::info!("ladybird worker: running");
        loop {
            match rx.recv() {
                Ok(cmd) => {
                    if self.handle(cmd) {
                        break;
                    }
                }
                Err(_) => {
                    tracing::info!("ladybird worker: channel closed, exiting");
                    break;
                }
            }
        }
        tracing::info!("ladybird worker: exited");
    }

    fn handle(&mut self, cmd: Command) -> bool {
        match cmd {
            Command::OpenTab { url, reply } => {
                let _ = reply.send(self.open_tab(&url));
            }
            Command::OpenTabAt {
                url,
                insert_idx,
                reply,
            } => {
                let _ = reply.send(self.open_tab_at(&url, insert_idx));
            }
            Command::CloseTab { id, reply } => {
                let _ = reply.send(self.close_tab(id));
            }
            Command::SelectTab { id } => {
                self.select_tab(id);
            }
            Command::CycleTab { forward } => {
                self.cycle_tab(forward);
            }
            Command::MoveTab { from, to } => {
                self.move_tab(from, to);
            }
            Command::Navigate { url, reply } => {
                let _ = reply.send(self.navigate(&url));
            }
            Command::GoBack => {
                self.go_back();
            }
            Command::GoForward => {
                self.go_forward();
            }
            Command::Reload => {
                self.reload();
            }
            Command::Stop => {
                self.stop();
            }
            Command::Resize { width, height } => {
                self.resize(width, height);
            }
            Command::ForcePaint => {
                self.paint();
            }
            Command::QueryTabs { reply } => {
                let _ = reply.send(self.build_snapshot());
            }
            Command::QueryCanGoBack { reply } => {
                let _ = reply.send(self.active_can_go_back());
            }
            Command::QueryCanGoForward { reply } => {
                let _ = reply.send(self.active_can_go_forward());
            }
            Command::SendKey {
                key_code,
                is_press,
                modifiers,
            } => {
                self.send_key(key_code, is_press, modifiers);
            }
            Command::SendMouseMove { x, y } => {
                self.send_mouse_move(x, y);
            }
            Command::SendMouseButton {
                button,
                x,
                y,
                is_press,
            } => {
                self.send_mouse_button(button, x, y, is_press);
            }
            Command::SendScroll { x, y, dx, dy } => {
                self.send_scroll(x, y, dx, dy);
            }
            Command::SetZoom(level) => {
                self.set_zoom(level);
            }
            Command::QueryActiveZoom { reply } => {
                let _ = reply.send(self.query_active_zoom());
            }
            Command::ImeSetComposition {
                text,
                sel_start,
                sel_end,
            } => {
                tracing::debug!(
                    text = %text,
                    sel_start,
                    sel_end,
                    "ladybird worker: ImeSetComposition"
                );
                self.ime_set_composition(&text, sel_start, sel_end);
            }
            Command::ImeCommit { text } => {
                tracing::debug!(text = %text, "ladybird worker: ImeCommit");
                self.ime_commit(&text);
            }
            Command::ImeCancel => {
                tracing::debug!("ladybird worker: ImeCancel");
                self.ime_cancel();
            }
            Command::EvalJs { code } => {
                self.eval_js(&code);
            }
            Command::EvalMainFrameJs { code, url } => {
                self.eval_main_frame_js(&code, &url);
            }
            Command::EditAttach { field_id } => {
                self.edit_attach(&field_id);
            }
            Command::EditCycle { forward } => {
                self.edit_cycle(forward);
            }
            Command::EditDetach { field_id } => {
                self.edit_detach(&field_id);
            }
            Command::EditFocus { field_id } => {
                self.edit_focus(&field_id);
            }
            Command::QueryAnyAudioActive { reply } => {
                let _ = reply.send(self.any_audio_active());
            }
            Command::QueryAnyVideoActive { reply } => {
                let _ = reply.send(self.any_video_active());
            }
            Command::SetSleep { sleep } => {
                self.set_sleep(sleep);
            }
            Command::StartDownload { url } => {
                self.start_download(&url);
            }
            Command::QueryIsLoading { reply } => {
                let _ = reply.send(self.query_is_loading());
            }
            Command::DrainFavicons { reply } => {
                // Phase B: no favicon tracking — return empty Vec.
                // Phase C: drain a shared queue populated by on_favicon_change callbacks.
                let _ = reply.send(Vec::new());
            }
            Command::Shutdown => {
                return true;
            }
        }
        false
    }
}

// ── Spawn helper ──────────────────────────────────────────────────────────────

pub(crate) fn spawn(
    initial_url: &str,
    width: u32,
    height: u32,
    frame: SharedOsrFrame,
    view: SharedOsrViewState,
) -> Result<WorkerHandle, LadybirdError> {
    let (tx, rx) = mpsc::sync_channel::<Command>(64);
    let initial_url = initial_url.to_owned();

    thread::Builder::new()
        .name("buffr-ladybird-worker".into())
        .spawn(move || {
            tracing::info!("ladybird worker: starting");
            let mut worker = Worker::new(Arc::clone(&frame), Arc::clone(&view), width, height);
            if let Err(e) = worker.open_tab(&initial_url) {
                tracing::error!("ladybird worker: failed to open initial tab: {e}");
            }
            worker.run(rx);
        })
        .map_err(|e| LadybirdError::InitFailed(e.to_string()))?;

    Ok(WorkerHandle { tx })
}
