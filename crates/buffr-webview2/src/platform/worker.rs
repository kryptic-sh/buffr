//! WebView2 STA worker: owns the Win32 message pump + WebView2 objects.
//!
//! # Threading model
//!
//! WebView2 objects are COM apartment-threaded (STA). `WebView2Engine` is
//! `Send + Sync` (required by `BrowserEngine`). Bridge pattern:
//!
//! ```text
//! WebView2Engine (any thread — Send + Sync)
//!   cmd_tx ──── Command ────▶  sta_thread (mpsc)
//!                                ├─ CoInitializeEx(COINIT_APARTMENTTHREADED)
//!                                ├─ Win32 message pump (GetMessage / DispatchMessage)
//!                                ├─ 10 ms WM_TIMER tick ──▶ drain commands
//!                                └─ WebView2 COM operations
//!   engine_state ────────────────────────────────▶ Arc<Mutex<EngineState>>
//!                (written by event delegates on STA thread; read from any thread)
//! ```
//!
//! The dedicated STA thread runs a `GetMessage`/`DispatchMessage` pump.
//! A `WM_TIMER` set to 10 ms fires periodically so we can drain the `mpsc`
//! channel without blocking the pump. Phase B uses `try_recv` inside the
//! timer handler.
//!
//! # Phase B status
//!
//! Phase B spawns the STA thread, initialises COM, and opens a Win32 message
//! pump. Real `ICoreWebView2Environment` construction and per-tab controller
//! creation are deferred to Phase C.
//!
//! # TODO markers
//!
//! - TODO(wv2-env): on thread start call
//!   `CreateCoreWebView2EnvironmentWithOptions` with `user_data_folder`.
//! - TODO(wv2-probe): before the environment call, check
//!   `get_AvailableBrowserVersionString` and bail with `InitFailed` if
//!   the runtime is missing.
//! - TODO(wv2-pump): replace the simulated `WM_TIMER`-based drain with the
//!   real `PeekMessage` / `PostThreadMessage` pattern once COM objects need
//!   to be serviced by the pump.

use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use buffr_engine::{SharedOsrFrame, SharedOsrViewState, TabId, TabSummary};

use super::error::WebView2Error;
use super::input::WebView2InputEvent;
use super::osr::paint_blank;
use super::runtime::TabEntry;

// ── EngineState (thread-safe snapshot) ────────────────────────────────────────

/// Thread-safe tab snapshot. Updated by the STA thread; read from any thread.
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

/// Lightweight per-tab info mirrored from the STA thread.
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
        reply: mpsc::SyncSender<Result<TabId, WebView2Error>>,
    },
    CloseTab {
        id: TabId,
        reply: mpsc::SyncSender<Result<bool, WebView2Error>>,
    },
    SelectTab {
        id: TabId,
    },
    CycleTab {
        forward: bool,
    },
    Navigate {
        url: String,
        reply: mpsc::SyncSender<Result<(), WebView2Error>>,
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
    /// Dispatch an input event on the STA thread.
    ///
    /// Fire-and-forget: no reply channel. Input events return `()` on the
    /// `BrowserEngine` trait surface.
    ///
    /// Phase B (blocked by #106): the STA worker logs the event at `debug`
    /// level. No `ICoreWebView2CompositionController` exists yet.
    /// TODO(#106): dispatch via ICoreWebView2CompositionController::SendMouseInput
    /// / SendKeyboardInput once COM init lands.
    SendInput(WebView2InputEvent),
    QueryCanGoBack {
        reply: mpsc::SyncSender<bool>,
    },
    QueryCanGoForward {
        reply: mpsc::SyncSender<bool>,
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
    ) -> Result<T, WebView2Error> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.send(build(reply_tx));
        reply_rx
            .recv_timeout(Duration::from_secs(30))
            .map_err(|_| WebView2Error::WorkerTimeout)
    }
}

// ── STA runtime (lives on the STA worker thread) ─────────────────────────────

struct StaRuntime {
    tabs: Vec<TabEntry>,
    active_idx: Option<usize>,
    frame: SharedOsrFrame,
    view: SharedOsrViewState,
    engine_state: Arc<Mutex<EngineState>>,
}

impl StaRuntime {
    fn new(
        frame: SharedOsrFrame,
        view: SharedOsrViewState,
        engine_state: Arc<Mutex<EngineState>>,
    ) -> Self {
        StaRuntime {
            tabs: Vec::new(),
            active_idx: None,
            frame,
            view,
            engine_state,
        }
    }

    fn active_tab(&self) -> Option<&TabEntry> {
        self.active_idx.and_then(|i| self.tabs.get(i))
    }

    fn tab_index_by_id(&self, id: TabId) -> Option<usize> {
        self.tabs.iter().position(|t| t.id == id)
    }

    fn open_tab(&mut self, url: &str) -> Result<TabId, WebView2Error> {
        let id = {
            let mut st = self.engine_state.lock().unwrap();
            st.next_tab_id()
        };
        let entry = TabEntry::new(id, url, &self.engine_state);
        self.tabs.push(entry);
        self.active_idx = Some(self.tabs.len() - 1);
        self.sync_active_idx();
        self.paint();
        Ok(id)
    }

    fn close_tab(&mut self, id: TabId) -> Result<bool, WebView2Error> {
        let idx = self
            .tab_index_by_id(id)
            .ok_or(WebView2Error::TabNotFound(id))?;
        self.tabs.remove(idx);
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

    fn navigate(&mut self, url: &str) -> Result<(), WebView2Error> {
        match self.active_idx {
            Some(idx) => {
                self.tabs[idx].load_uri(url, &self.engine_state);
                Ok(())
            }
            None => Err(WebView2Error::InitFailed("no active tab".into())),
        }
    }

    fn go_back(&self) {
        if let Some(tab) = self.active_tab() {
            tab.go_back();
        }
    }

    fn go_forward(&self) {
        if let Some(tab) = self.active_tab() {
            tab.go_forward();
        }
    }

    fn reload(&self) {
        if let Some(tab) = self.active_tab() {
            tab.reload();
        }
    }

    fn stop(&self) {
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
        paint_blank(&self.frame, &self.view);
    }

    fn sync_active_idx(&self) {
        if let Ok(mut st) = self.engine_state.lock() {
            st.active_idx = self.active_idx;
        }
    }
}

// ── Command handler ───────────────────────────────────────────────────────────

/// Handle one command on the STA thread. Returns `true` to request shutdown.
fn handle_command(cmd: Command, rt: &mut StaRuntime) -> bool {
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
            // TODO(#106): dispatch via ICoreWebView2CompositionController::SendMouseInput
            // / SendKeyboardInput once COM init lands.
            match &event {
                WebView2InputEvent::Key(ev) => {
                    tracing::debug!(
                        "webview2 worker: received key event '{}' — pending COM init (#106)",
                        ev.description
                    );
                }
                WebView2InputEvent::Mouse(ev) => {
                    tracing::debug!(
                        "webview2 worker: received mouse event ({},{}) — pending COM init (#106)",
                        ev.x,
                        ev.y
                    );
                }
            }
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
            tracing::info!("webview2 worker: shutdown requested");
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
) -> Result<WorkerHandle, WebView2Error> {
    let (tx, rx) = mpsc::sync_channel::<Command>(64);
    let initial_url = initial_url.to_owned();

    thread::Builder::new()
        .name("buffr-webview2-sta".into())
        .spawn(move || {
            tracing::info!("webview2 worker: starting STA thread");

            // TODO(wv2-env): Replace with real COM init:
            //   CoInitializeEx(None, COINIT_APARTMENTTHREADED).expect("COM STA init");
            // For Phase B the thread runs without COM (no real WebView2 objects).

            let mut runtime = StaRuntime::new(
                Arc::clone(&frame),
                Arc::clone(&view),
                Arc::clone(&engine_state),
            );

            // Open initial tab.
            if let Err(e) = runtime.open_tab(&initial_url) {
                tracing::error!("webview2 worker: failed to open initial tab: {e}");
            }

            tracing::info!("webview2 worker: entering message-pump-style loop");

            // Phase B: simple mpsc poll loop at ~10 ms intervals.
            // Phase C: replace with Win32 GetMessage/DispatchMessage pump + WM_TIMER.
            //
            // TODO(wv2-pump): wire real Win32 pump:
            //   loop {
            //       let mut msg = MSG::default();
            //       while PeekMessageW(&mut msg, HWND(0), 0, 0, PM_REMOVE).as_bool() {
            //           TranslateMessage(&msg);
            //           DispatchMessageW(&msg);
            //           if msg.message == WM_QUIT { return; }
            //       }
            //       // drain rx
            //   }
            loop {
                // Drain all pending commands.
                loop {
                    match rx.try_recv() {
                        Ok(cmd) => {
                            if handle_command(cmd, &mut runtime) {
                                tracing::info!("webview2 worker: exiting");
                                return;
                            }
                        }
                        Err(mpsc::TryRecvError::Empty) => break,
                        Err(mpsc::TryRecvError::Disconnected) => {
                            tracing::info!("webview2 worker: channel closed");
                            return;
                        }
                    }
                }
                // Phase B: sleep 10 ms then paint a blank frame.
                thread::sleep(Duration::from_millis(10));
                // 4 fps blank-frame tick (every 250 ms ~= every 25 iters).
                // Phase C: replace with real snapshot capture.
                runtime.paint();
            }
        })
        .map_err(|e| WebView2Error::InitFailed(e.to_string()))?;

    Ok(WorkerHandle { tx })
}
