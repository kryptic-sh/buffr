//! WebKitGTK worker: owns the GTK main loop + WebView objects.
//!
//! # Threading model
//!
//! WebKitGTK / GTK4 WebView objects are main-thread-only. `WebKitGtkEngine`
//! is `Send + Sync` (required by `BrowserEngine`). Bridge pattern:
//!
//! ```text
//! WebKitGtkEngine (any thread — Send + Sync)
//!   cmd_tx ──── Command ────▶  gtk_thread (mpsc)
//!                                ├─ glib::MainLoop::run()
//!                                ├─ timeout_add_local (10 ms poll) ──▶ drain commands
//!                                └─ WebView operations
//!   engine_state ────────────────────────────────▶ Arc<Mutex<EngineState>>
//!                (written by WebView signal handlers on GTK thread)
//! ```
//!
//! The dedicated GTK thread runs `glib::MainLoop::run()`. A 10 ms
//! `timeout_add_local` tick polls the `mpsc::Receiver` for commands. Using
//! `timeout_add_local` (not `idle_add`) avoids the `Send` bound since the
//! closure runs on the same thread that installed it.

use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use buffr_engine::{SharedOsrFrame, SharedOsrViewState, TabId, TabSummary};

use super::error::WebKitGtkError;
use super::input::GtkInputEvent;
use super::osr::{paint_blank, request_snapshot};
use super::runtime::{OsrHandles, TabEntry};

// ── EngineState (thread-safe snapshot) ────────────────────────────────────────

/// Thread-safe tab snapshot. Updated by GTK signal handlers; read from any
/// thread by the engine behind `Mutex<EngineState>`.
pub(crate) struct EngineState {
    /// Open tabs in strip order.
    pub tabs: Vec<TabInfo>,
    /// Index of the active tab in `tabs`.
    pub active_idx: Option<usize>,
    /// Next tab id counter.
    pub next_id: u64,
    /// Current viewport width (pixels).
    pub width: u32,
    /// Current viewport height (pixels).
    pub height: u32,
}

impl EngineState {
    pub(crate) fn new(width: u32, height: u32) -> Self {
        EngineState {
            tabs: Vec::new(),
            active_idx: None,
            next_id: 1,
            width,
            height,
        }
    }

    pub(crate) fn next_tab_id(&mut self) -> TabId {
        let id = TabId(self.next_id);
        self.next_id += 1;
        id
    }

    pub(crate) fn summaries(&self) -> Vec<TabSummary> {
        self.tabs.iter().map(|t| t.to_summary()).collect()
    }
}

/// Lightweight per-tab info mirrored from the GTK thread.
#[derive(Clone)]
pub(crate) struct TabInfo {
    pub id: TabId,
    pub url: String,
    pub title: String,
    pub is_loading: bool,
    pub can_go_back: bool,
    pub can_go_forward: bool,
    pub progress: f64,
}

impl TabInfo {
    pub(crate) fn to_summary(&self) -> TabSummary {
        TabSummary {
            id: self.id,
            browser_id: 0,
            title: self.title.clone(),
            url: self.url.clone(),
            progress: self.progress as f32,
            is_loading: self.is_loading,
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
        reply: mpsc::SyncSender<Result<TabId, WebKitGtkError>>,
    },
    CloseTab {
        id: TabId,
        reply: mpsc::SyncSender<Result<bool, WebKitGtkError>>,
    },
    SelectTab {
        id: TabId,
    },
    CycleTab {
        forward: bool,
    },
    Navigate {
        url: String,
        reply: mpsc::SyncSender<Result<(), WebKitGtkError>>,
    },
    /// Navigate the active tab back. No return value needed.
    GoBack,
    /// Navigate the active tab forward. No return value needed.
    GoForward,
    /// Reload the active tab.
    Reload,
    /// Stop loading the active tab.
    Stop,
    Resize {
        width: u32,
        height: u32,
    },
    ForcePaint,
    /// Dispatch an input event to the active WebView.
    ///
    /// Fire-and-forget: no reply channel. Input events return `()` on the
    /// `BrowserEngine` trait surface.
    ///
    /// Phase B: the worker logs the event at `debug` level.
    /// TODO(input-key/input-mouse): synthesise real `gdk4::Event` and dispatch
    /// via `WebView::event()` when safe constructors are available (gdk4 >= 0.12).
    SendInput(GtkInputEvent),
    /// Full tab snapshot (used internally by QueryCanGoBack/Forward).
    QueryTabs {
        reply: mpsc::SyncSender<TabsSnapshot>,
    },
    QueryCanGoBack {
        reply: mpsc::SyncSender<bool>,
    },
    QueryCanGoForward {
        reply: mpsc::SyncSender<bool>,
    },
    Shutdown,
}

/// Snapshot sent back for QueryTabs.
#[allow(dead_code)]
pub(crate) struct TabsSnapshot {
    pub summaries: Vec<TabSummary>,
    pub active_idx: Option<usize>,
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
    ) -> Result<T, WebKitGtkError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.send(build(reply_tx));
        reply_rx
            .recv_timeout(Duration::from_secs(30))
            .map_err(|_| WebKitGtkError::WorkerTimeout)
    }
}

// ── GTK runtime ──────────────────────────────────────────────────────────────

/// State that lives entirely on the GTK main thread.
struct GtkRuntime {
    tabs: Vec<TabEntry>,
    active_idx: Option<usize>,
    frame: SharedOsrFrame,
    view: SharedOsrViewState,
    engine_state: Arc<Mutex<EngineState>>,
    /// `true` while a `WebView::snapshot()` call is in-flight.
    ///
    /// Prevents stacking up snapshot callbacks. Cleared by the callback
    /// regardless of success or failure. Wrapped in `Rc<Cell<bool>>` so it
    /// can be shared into the `snapshot` callback closure without `Send`.
    snapshot_in_flight: std::rc::Rc<std::cell::Cell<bool>>,
}

impl GtkRuntime {
    fn new(
        frame: SharedOsrFrame,
        view: SharedOsrViewState,
        engine_state: Arc<Mutex<EngineState>>,
    ) -> Self {
        GtkRuntime {
            tabs: Vec::new(),
            active_idx: None,
            frame,
            view,
            engine_state,
            snapshot_in_flight: std::rc::Rc::new(std::cell::Cell::new(false)),
        }
    }

    fn active_tab(&self) -> Option<&TabEntry> {
        self.active_idx.and_then(|i| self.tabs.get(i))
    }

    fn tab_index_by_id(&self, id: TabId) -> Option<usize> {
        self.tabs.iter().position(|t| t.id == id)
    }

    fn open_tab(&mut self, url: &str) -> Result<TabId, WebKitGtkError> {
        let id = {
            let mut st = self.engine_state.lock().unwrap();
            st.next_tab_id()
        };
        let (w, h) = {
            let st = self.engine_state.lock().unwrap();
            (st.width, st.height)
        };
        // TabEntry::new registers itself in engine_state.tabs.
        let entry = TabEntry::new(
            id,
            url,
            w,
            h,
            Arc::clone(&self.engine_state),
            OsrHandles {
                frame: Arc::clone(&self.frame),
                view: Arc::clone(&self.view),
                snapshot_in_flight: std::rc::Rc::clone(&self.snapshot_in_flight),
            },
        );
        self.tabs.push(entry);
        self.active_idx = Some(self.tabs.len() - 1);
        self.sync_active_idx();
        self.paint();
        Ok(id)
    }

    fn close_tab(&mut self, id: TabId) -> Result<bool, WebKitGtkError> {
        let idx = self
            .tab_index_by_id(id)
            .ok_or(WebKitGtkError::TabNotFound(id))?;
        self.tabs.remove(idx);
        // Also remove from engine_state.tabs.
        if let Ok(mut st) = self.engine_state.lock() {
            st.tabs.retain(|t| t.id != id);
        }
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
        self.sync_active_idx();
        self.paint();
        Ok(!self.tabs.is_empty())
    }

    fn select_tab(&mut self, id: TabId) {
        if let Some(idx) = self.tab_index_by_id(id) {
            self.active_idx = Some(idx);
            self.sync_active_idx();
            self.paint();
        }
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
        self.sync_active_idx();
        self.paint();
    }

    fn navigate(&mut self, url: &str) -> Result<(), WebKitGtkError> {
        match self.active_idx {
            Some(idx) => {
                self.tabs[idx].load_uri(url);
                Ok(())
            }
            None => Err(WebKitGtkError::InitFailed("no active tab".into())),
        }
    }

    fn go_back(&mut self) {
        if let Some(tab) = self.active_tab() {
            tab.go_back();
        }
    }

    fn go_forward(&mut self) {
        if let Some(tab) = self.active_tab() {
            tab.go_forward();
        }
    }

    fn reload(&mut self) {
        if let Some(tab) = self.active_tab() {
            tab.reload();
        }
    }

    fn stop(&mut self) {
        if let Some(tab) = self.active_tab() {
            tab.stop();
        }
    }

    fn resize(&mut self, width: u32, height: u32) {
        {
            let mut st = self.engine_state.lock().unwrap();
            st.width = width;
            st.height = height;
        }
        use std::sync::atomic::Ordering;
        self.view.width.store(width, Ordering::Relaxed);
        self.view.height.store(height, Ordering::Relaxed);
        self.paint();
    }

    fn paint(&self) {
        if let Some(tab) = self.active_tab() {
            request_snapshot(
                &tab.web_view,
                Arc::clone(&self.frame),
                Arc::clone(&self.view),
                std::rc::Rc::clone(&self.snapshot_in_flight),
            );
        } else {
            paint_blank(&self.frame, &self.view);
        }
    }

    /// Write active_idx into the shared EngineState.
    fn sync_active_idx(&self) {
        if let Ok(mut st) = self.engine_state.lock() {
            st.active_idx = self.active_idx;
        }
    }
}

// ── Command handler ───────────────────────────────────────────────────────────

/// Handle one command. Returns `true` to request shutdown.
fn handle_command(cmd: Command, rt: &mut GtkRuntime, main_loop: &glib::MainLoop) -> bool {
    match cmd {
        Command::OpenTab { url, reply } => {
            let _ = reply.send(rt.open_tab(&url));
        }
        Command::CloseTab { id, reply } => {
            let _ = reply.send(rt.close_tab(id));
        }
        Command::SelectTab { id } => {
            rt.select_tab(id);
        }
        Command::CycleTab { forward } => {
            rt.cycle_tab(forward);
        }
        Command::Navigate { url, reply } => {
            let _ = reply.send(rt.navigate(&url));
        }
        Command::GoBack => {
            rt.go_back();
        }
        Command::GoForward => {
            rt.go_forward();
        }
        Command::Reload => {
            rt.reload();
        }
        Command::Stop => {
            rt.stop();
        }
        Command::Resize { width, height } => {
            rt.resize(width, height);
        }
        Command::ForcePaint => {
            rt.paint();
        }
        Command::SendInput(event) => {
            // Phase B: log the event on the GTK thread (the correct dispatch queue).
            // GTK4 / gdk4 0.11 does not expose safe synthetic-event constructors;
            // real dispatch requires unsafe FFI or gdk4 >= 0.12.
            // TODO(input-key/input-mouse): synthesise gdk4::Event and call
            // WebView::event() once safe constructors are available.
            match &event {
                GtkInputEvent::Key(ev) => {
                    tracing::debug!(
                        "webkitgtk worker: received key event '{}' (pending gdk4 dispatch)",
                        ev.description
                    );
                }
                GtkInputEvent::Mouse(ev) => {
                    tracing::debug!(
                        "webkitgtk worker: received mouse event ({:.0},{:.0}) (pending gdk4 dispatch)",
                        ev.x,
                        ev.y
                    );
                }
            }
        }
        Command::QueryTabs { reply } => {
            let summaries = rt
                .engine_state
                .lock()
                .map(|st| st.summaries())
                .unwrap_or_default();
            let active_idx = rt.engine_state.lock().ok().and_then(|st| st.active_idx);
            let _ = reply.send(TabsSnapshot {
                summaries,
                active_idx,
            });
        }
        Command::QueryCanGoBack { reply } => {
            let result = rt.active_tab().map(|t| t.can_go_back()).unwrap_or(false);
            let _ = reply.send(result);
        }
        Command::QueryCanGoForward { reply } => {
            let result = rt.active_tab().map(|t| t.can_go_forward()).unwrap_or(false);
            let _ = reply.send(result);
        }
        Command::Shutdown => {
            tracing::info!("webkitgtk worker: shutdown requested");
            main_loop.quit();
            return true;
        }
    }
    false
}

// ── Spawn ─────────────────────────────────────────────────────────────────────

pub(crate) fn spawn(
    initial_url: &str,
    _width: u32,
    _height: u32,
    frame: SharedOsrFrame,
    view: SharedOsrViewState,
    engine_state: Arc<Mutex<EngineState>>,
) -> Result<WorkerHandle, WebKitGtkError> {
    // gtk4::init() must be called before any GTK widgets are created.
    // It is safe to call multiple times; subsequent calls are no-ops.
    gtk4::init().map_err(|e| WebKitGtkError::InitFailed(e.to_string()))?;

    let (tx, rx) = mpsc::sync_channel::<Command>(64);
    let initial_url = initial_url.to_owned();

    thread::Builder::new()
        .name("buffr-webkitgtk-worker".into())
        .spawn(move || {
            tracing::info!("webkitgtk worker: starting");

            // Build the GLib main loop on this thread's default context.
            let main_loop = glib::MainLoop::new(None, false);

            // Wrap GtkRuntime in an Rc<RefCell<>> so it can be shared
            // across the timeout closures (all run on this thread).
            use std::cell::RefCell;
            use std::rc::Rc;

            let mut runtime = GtkRuntime::new(
                Arc::clone(&frame),
                Arc::clone(&view),
                Arc::clone(&engine_state),
            );

            // Open initial tab.
            if let Err(e) = runtime.open_tab(&initial_url) {
                tracing::error!("webkitgtk worker: failed to open initial tab: {e}");
            }

            // Wrap in Rc<RefCell<>> for shared access across closures.
            let runtime_rc = Rc::new(RefCell::new(runtime));

            // ── 4 fps snapshot tick (250 ms) ──────────────────────────────
            //
            // Calls `WebView::snapshot()` on the active tab every 250 ms.
            // If a snapshot is already in-flight the tick is a no-op (the
            // `in_flight` flag is checked inside `request_snapshot`).
            // Falls back to `paint_blank` when no tab is open yet.
            {
                let rt_rc_snap = Rc::clone(&runtime_rc);
                glib::timeout_add_local(Duration::from_millis(250), move || {
                    let rt = rt_rc_snap.borrow();
                    rt.paint();
                    glib::ControlFlow::Continue
                });
            }

            // ── Command poll tick (10 ms) ─────────────────────────────────
            //
            // Drains the mpsc channel from the GTK thread. `timeout_add_local`
            // closures run on the same thread — no Send bound needed.
            {
                let ml = main_loop.clone();
                let rt_rc = Rc::clone(&runtime_rc);
                glib::timeout_add_local(Duration::from_millis(10), move || {
                    // Drain all pending commands in a tight batch.
                    loop {
                        match rx.try_recv() {
                            Ok(cmd) => {
                                if handle_command(cmd, &mut rt_rc.borrow_mut(), &ml) {
                                    return glib::ControlFlow::Break;
                                }
                            }
                            Err(mpsc::TryRecvError::Empty) => {
                                return glib::ControlFlow::Continue;
                            }
                            Err(mpsc::TryRecvError::Disconnected) => {
                                tracing::info!("webkitgtk worker: channel closed");
                                ml.quit();
                                return glib::ControlFlow::Break;
                            }
                        }
                    }
                });
            }

            tracing::info!("webkitgtk worker: entering GLib main loop");
            main_loop.run();
            tracing::info!("webkitgtk worker: main loop exited");
        })
        .map_err(|e| WebKitGtkError::InitFailed(e.to_string()))?;

    Ok(WorkerHandle { tx })
}
