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
    WebContent, webcontent_can_go_back, webcontent_can_go_forward, webcontent_go_back,
    webcontent_go_forward, webcontent_ime_cancel, webcontent_ime_commit,
    webcontent_ime_set_composition, webcontent_navigate, webcontent_new, webcontent_read_pixels,
    webcontent_reload, webcontent_resize, webcontent_send_key, webcontent_send_mouse_button,
    webcontent_send_mouse_move, webcontent_send_scroll, webcontent_set_zoom, webcontent_stop,
    webcontent_title, webcontent_url, webcontent_zoom,
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
            Command::CloseTab { id, reply } => {
                let _ = reply.send(self.close_tab(id));
            }
            Command::SelectTab { id } => {
                self.select_tab(id);
            }
            Command::CycleTab { forward } => {
                self.cycle_tab(forward);
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
