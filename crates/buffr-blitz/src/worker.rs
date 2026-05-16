//! Blitz worker thread.
//!
//! `HtmlDocument` (and Stylo's `BaseDocument`) is `!Send + !Sync`.  This
//! module runs a dedicated thread that owns all Blitz documents and exposes
//! them via `mpsc` commands from `BlitzEngine`.
//!
//! # OSR pipeline
//!
//! After every document mutation (open / navigate / reload / resize / input)
//! we rasterise the active tab with the CPU Vello renderer via
//! [`crate::render::render_doc_into_frame`] and write premultiplied BGRA8
//! pixels into `SharedOsrFrame`.  On error, a solid white fill is used as
//! fallback.

use std::sync::atomic::Ordering;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use buffr_engine::{SharedOsrFrame, SharedOsrViewState, TabId, TabSummary};

use blitz_dom::Document;

use crate::document::BlitzTab;
use crate::error::BlitzError;
use crate::input::BlitzInputEvent;
use crate::render::{render_doc_into_frame, write_white_fill};

// ── Tab snapshot ──────────────────────────────────────────────────────────────

/// Lightweight data about a single tab, safe to send across threads.
pub(crate) struct TabRecord {
    pub id: TabId,
    pub url: String,
    pub title: String,
    pub is_loading: bool,
    /// Used by `can_go_back` query; not read from the snapshot struct itself.
    #[allow(dead_code)]
    pub can_go_back: bool,
    /// Used by `can_go_forward` query; not read from the snapshot struct itself.
    #[allow(dead_code)]
    pub can_go_forward: bool,
}

impl TabRecord {
    fn from_tab(tab: &BlitzTab) -> Self {
        TabRecord {
            id: tab.id,
            url: tab.url.clone(),
            title: tab.title.clone(),
            is_loading: tab.is_loading,
            can_go_back: tab.can_go_back(),
            can_go_forward: tab.can_go_forward(),
        }
    }

    pub(crate) fn to_summary(&self) -> TabSummary {
        TabSummary {
            id: self.id,
            browser_id: 0,
            title: self.title.clone(),
            url: self.url.clone(),
            progress: 1.0,
            is_loading: self.is_loading,
            pinned: false,
            private: false,
        }
    }
}

/// Snapshot of tab state, sent back from the worker.
pub(crate) struct TabsSnapshot {
    pub tabs: Vec<TabRecord>,
    pub active: Option<TabId>,
}

// ── Commands ──────────────────────────────────────────────────────────────────

#[allow(dead_code)]
pub(crate) enum Command {
    OpenTab {
        url: String,
        reply: mpsc::SyncSender<Result<TabId, BlitzError>>,
    },
    /// Open a tab and insert it at `insert_idx` (clamped to len).
    OpenTabAt {
        url: String,
        insert_idx: usize,
        reply: mpsc::SyncSender<Result<TabId, BlitzError>>,
    },
    CloseTab {
        id: TabId,
        reply: mpsc::SyncSender<Result<bool, BlitzError>>,
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
        reply: mpsc::SyncSender<Result<(), BlitzError>>,
    },
    GoBack {
        n: usize,
    },
    GoForward {
        n: usize,
    },
    Reload,
    Stop,
    /// Set the active tab's CSS zoom level. Clamped to [0.25, 5.0].
    SetZoom(f64),
    Resize {
        width: u32,
        height: u32,
    },
    ForcePaint,
    /// Dispatch an input event to the active tab's document.
    ///
    /// Fire-and-forget: no reply channel.  Input events return `()` on the
    /// `BrowserEngine` trait surface and are handled asynchronously.
    SendInput(BlitzInputEvent),
    QueryTabs {
        reply: mpsc::SyncSender<TabsSnapshot>,
    },
    QueryCanGoBack {
        reply: mpsc::SyncSender<bool>,
    },
    QueryCanGoForward {
        reply: mpsc::SyncSender<bool>,
    },
    /// Query the active tab's zoom multiplier (defaults 1.0).
    QueryActiveZoom {
        reply: mpsc::SyncSender<f64>,
    },
    Shutdown,
}

// ── WorkerHandle ──────────────────────────────────────────────────────────────

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
    ) -> Result<T, BlitzError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.send(build(reply_tx));
        reply_rx
            .recv_timeout(Duration::from_secs(30))
            .map_err(|_| BlitzError::FetchFailed("worker timeout".into()))
    }
}

// ── Worker ────────────────────────────────────────────────────────────────────

struct Worker {
    tabs: Vec<BlitzTab>,
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

    fn next_tab_id(&mut self) -> TabId {
        let id = TabId(self.next_id);
        self.next_id += 1;
        id
    }

    fn tab_index_by_id(&self, id: TabId) -> Option<usize> {
        self.tabs.iter().position(|t| t.id == id)
    }

    fn open_tab(&mut self, url: &str) -> Result<TabId, BlitzError> {
        let id = self.next_tab_id();
        tracing::info!("blitz worker: open_tab {id} → {url}");
        let tab = BlitzTab::open(id, url, self.width, self.height)?;
        self.tabs.push(tab);
        self.active_idx = Some(self.tabs.len() - 1);
        self.paint();
        Ok(id)
    }

    /// Open a tab at `insert_idx` (appended first, then rotated into position).
    fn open_tab_at(&mut self, url: &str, insert_idx: usize) -> Result<TabId, BlitzError> {
        let id = self.open_tab(url)?;
        // Tab was appended at len-1; now rotate it to `insert_idx`.
        let appended = self.tabs.len() - 1;
        let clamped = insert_idx.min(appended);
        if clamped != appended {
            let tab = self.tabs.remove(appended);
            self.tabs.insert(clamped, tab);
            // active_idx was set to appended; update to clamped.
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
        // Stash active id before mutation so we can fix up active_idx.
        let active_id = self.active_idx.and_then(|i| self.tabs.get(i).map(|t| t.id));
        let tab = self.tabs.remove(from);
        self.tabs.insert(clamped_to, tab);
        // Re-derive active_idx from the stashed id.
        if let Some(aid) = active_id {
            self.active_idx = self.tabs.iter().position(|t| t.id == aid);
        }
        self.paint();
    }

    fn close_tab(&mut self, id: TabId) -> Result<bool, BlitzError> {
        let idx = self
            .tab_index_by_id(id)
            .ok_or(BlitzError::TabNotFound(id))?;
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
        if let Some(idx) = self.tab_index_by_id(id) {
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

    fn navigate(&mut self, url: &str) -> Result<(), BlitzError> {
        let (w, h) = (self.width, self.height);
        if let Some(idx) = self.active_idx {
            self.tabs[idx].navigate(url, w, h);
            self.paint();
            Ok(())
        } else {
            Err(BlitzError::InitFailed("no active tab".into()))
        }
    }

    fn go_back(&mut self, n: usize) {
        let (w, h) = (self.width, self.height);
        if let Some(idx) = self.active_idx {
            self.tabs[idx].go_back(n, w, h);
            self.paint();
        }
    }

    fn go_forward(&mut self, n: usize) {
        let (w, h) = (self.width, self.height);
        if let Some(idx) = self.active_idx {
            self.tabs[idx].go_forward(n, w, h);
            self.paint();
        }
    }

    fn reload(&mut self) {
        let (w, h) = (self.width, self.height);
        if let Some(idx) = self.active_idx {
            self.tabs[idx].reload(w, h);
            self.paint();
        }
    }

    fn stop(&self) {
        // Blitz has no async loading; no-op.
        tracing::debug!("blitz worker: stop — no-op (synchronous load model)");
    }

    /// Dispatch an input event to the active tab's `HtmlDocument`.
    ///
    /// `HtmlDocument` implements `blitz_dom::Document`, which exposes
    /// `handle_ui_event(UiEvent)` — the canonical entry point for pointer and
    /// keyboard events in `blitz-dom 0.3.0-alpha.2`.
    ///
    /// Repaints after the event so pointer/keyboard state changes are
    /// reflected in the OSR frame.
    fn send_input(&mut self, event: BlitzInputEvent) {
        if let Some(idx) = self.active_idx {
            let ui_event = event.into_ui_event();
            tracing::debug!(
                "blitz worker: dispatching ui_event to tab {}",
                self.tabs[idx].id
            );
            self.tabs[idx].doc.handle_ui_event(ui_event);
            self.paint();
        } else {
            tracing::debug!("blitz worker: send_input — no active tab, dropping event");
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

    fn build_snapshot(&self) -> TabsSnapshot {
        let tabs: Vec<TabRecord> = self.tabs.iter().map(TabRecord::from_tab).collect();
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

    /// Set the active tab's CSS zoom level. Clamped to [0.25, 5.0].
    /// Triggers a repaint so the change is visible immediately.
    fn set_zoom(&mut self, level: f64) {
        let clamped = level.clamp(0.25, 5.0);
        if let Some(idx) = self.active_idx {
            self.tabs[idx].zoom = clamped;
            self.paint();
        }
    }

    /// Rasterise the active tab into `SharedOsrFrame`.
    ///
    /// Uses the CPU Vello renderer via [`render_doc_into_frame`].  If there is
    /// no active tab, or the renderer returns an error, falls back to a solid
    /// white fill so the frame is never left stale.
    fn paint(&self) {
        let w = self.view.width.load(Ordering::Relaxed);
        let h = self.view.height.load(Ordering::Relaxed);
        let scale = self.view.scale();

        if let Some(idx) = self.active_idx {
            // Final paint scale = device-pixel-ratio × per-tab CSS zoom.
            let paint_scale = scale as f64 * self.tabs[idx].zoom;
            render_doc_into_frame(&self.tabs[idx].doc, &self.frame, w, h, paint_scale);
        } else {
            write_white_fill(&self.frame, w, h);
        }

        if let Some(wake) = self.view.wake.get() {
            wake();
        }
    }

    fn run(mut self, rx: mpsc::Receiver<Command>) {
        tracing::info!("blitz worker: running");
        loop {
            match rx.recv() {
                Ok(cmd) => {
                    if self.handle_command(cmd) {
                        break;
                    }
                }
                Err(_) => {
                    tracing::info!("blitz worker: channel closed, exiting");
                    break;
                }
            }
        }
        tracing::info!("blitz worker: exited");
    }

    fn handle_command(&mut self, cmd: Command) -> bool {
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
            Command::GoBack { n } => {
                self.go_back(n);
            }
            Command::GoForward { n } => {
                self.go_forward(n);
            }
            Command::Reload => {
                self.reload();
            }
            Command::Stop => {
                self.stop();
            }
            Command::SetZoom(level) => {
                self.set_zoom(level);
            }
            Command::Resize { width, height } => {
                self.resize(width, height);
            }
            Command::ForcePaint => {
                self.paint();
            }
            Command::SendInput(event) => {
                self.send_input(event);
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
            Command::QueryActiveZoom { reply } => {
                let z = self.active_idx.map(|i| self.tabs[i].zoom).unwrap_or(1.0);
                let _ = reply.send(z);
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
) -> Result<WorkerHandle, BlitzError> {
    let (tx, rx) = mpsc::sync_channel::<Command>(64);
    let initial_url = initial_url.to_owned();

    thread::Builder::new()
        .name("buffr-blitz-worker".into())
        .spawn(move || {
            tracing::info!("blitz worker: starting");
            // Catch worker panics so the failure surfaces as a log line
            // instead of a silent channel disconnect — otherwise the
            // engine side sees `Disconnected` and reports a misleading
            // "worker timeout" via recv_timeout's error mapping.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut worker = Worker::new(Arc::clone(&frame), Arc::clone(&view), width, height);
                if let Err(e) = worker.open_tab(&initial_url) {
                    tracing::error!("blitz worker: failed to open initial tab: {e}");
                }
                worker.run(rx);
            }));
            if let Err(payload) = result {
                let msg = payload
                    .downcast_ref::<&'static str>()
                    .map(|s| s.to_string())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "<non-string panic payload>".to_string());
                tracing::error!("blitz worker: panicked: {msg}");
                eprintln!("blitz worker thread panic: {msg}");
            }
        })
        .map_err(|e| BlitzError::InitFailed(e.to_string()))?;

    Ok(WorkerHandle { tx })
}
