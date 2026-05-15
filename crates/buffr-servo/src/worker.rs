//! Servo worker thread — shape-complete skeleton.
//!
//! The actual Servo runtime (`servo::Servo`, `servo::WebView`,
//! `servo::rendering_context::SoftwareRenderingContext`) cannot be imported
//! until the `libsqlite3-sys` dep conflict is resolved.  See `Cargo.toml`
//! for the full explanation.
//!
//! # What this module provides (Phase B skeleton)
//!
//! - [`Command`] enum with the full command surface a real worker would need
//! - [`TabsSnapshot`] / [`ServoTab`] — data structures mirroring the worker's
//!   internal state
//! - [`WorkerHandle`] — the `mpsc::SyncSender<Command>` wrapper that
//!   `ServoEngine` holds
//! - [`spawn()`] — starts a no-op worker thread that drains commands and
//!   logs them; real Servo calls are `// TODO`-commented at every callsite
//!
//! # When the dep conflict is fixed
//!
//! 1. Uncomment the `servo`, `keyboard-types`, `euclid`, `dpi` deps in
//!    `Cargo.toml`.
//! 2. Restore the `use` blocks marked `// TODO` below.
//! 3. Replace the `// TODO: call servo.spin_event_loop()` stubs in `run()`.
//! 4. Replace the `// TODO: call wv.*` stubs in `Worker::*` methods.
//!
//! # TODO: restore these imports when dep conflict is resolved
//!
//! ```rust,ignore
//! use std::rc::Rc;
//! use dpi::PhysicalSize;
//! use euclid::{Box2D, Point2D};
//! use servo::ServoBuilder;
//! use servo::rendering_context::SoftwareRenderingContext;
//! use servo::webview::{WebView, WebViewBuilder, WebViewDelegate, LoadStatus};
//! use url::Url;
//! ```

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread;
use std::time::Duration;

use buffr_engine::{SharedOsrFrame, SharedOsrViewState, TabId, TabSummary};

use crate::error::ServoError;
use crate::input::ServoInputEvent;

// ── Tab record ────────────────────────────────────────────────────────────────

/// One tab's in-engine state, mirroring what the real worker would maintain.
#[derive(Clone)]
pub(crate) struct ServoTab {
    pub id: TabId,
    pub url: String,
    pub title: String,
    pub is_loading: bool,
    // TODO: add `webview: Option<servo::WebView>` when dep conflict is resolved
}

impl ServoTab {
    pub(crate) fn to_summary(&self) -> TabSummary {
        TabSummary {
            id: self.id,
            browser_id: 0,
            title: self.title.clone(),
            url: self.url.clone(),
            progress: if self.is_loading { 0.5 } else { 1.0 },
            is_loading: self.is_loading,
            pinned: false,
            private: false,
        }
    }
}

// ── Commands ──────────────────────────────────────────────────────────────────

/// Commands the engine sends to the worker.
///
/// The full command surface is defined here so the skeleton already matches
/// what the real implementation will need.  Some variants are not yet wired
/// from `ServoEngine` (they will be in Phase C once the dep conflict is fixed).
#[allow(dead_code)]
pub(crate) enum Command {
    /// Open a new tab and navigate to `url`.
    OpenTab {
        url: String,
        reply: mpsc::SyncSender<Result<TabId, ServoError>>,
    },
    /// Close the tab with `id`.
    CloseTab {
        id: TabId,
        reply: mpsc::SyncSender<Result<bool, ServoError>>,
    },
    /// Switch the active tab.
    SelectTab { id: TabId },
    /// Cycle forward/backward.
    CycleTab { forward: bool },
    /// Navigate the active tab.
    Navigate {
        url: String,
        reply: mpsc::SyncSender<Result<(), ServoError>>,
    },
    /// Navigate active tab back `n` steps.
    GoBack { n: usize },
    /// Navigate active tab forward `n` steps.
    GoForward { n: usize },
    /// Reload active tab.
    Reload,
    /// Stop loading active tab.
    Stop,
    /// Resize the OSR viewport.
    Resize { width: u32, height: u32 },
    /// Deliver an input event to the active tab.
    ///
    /// TODO: change `ServoInputEvent` to `servo::input_events::InputEvent`
    /// when dep conflict is resolved.
    SendInput { event: ServoInputEvent },
    /// Snapshot the tab list.
    QueryTabs {
        reply: mpsc::SyncSender<TabsSnapshot>,
    },
    /// Shut down the worker.
    Shutdown,
}

/// Snapshot of tab state, sent back from the worker.
pub(crate) struct TabsSnapshot {
    pub tabs: Vec<ServoTab>,
    pub active: Option<TabId>,
}

// ── WorkerHandle ──────────────────────────────────────────────────────────────

/// Handle to the worker thread.
#[derive(Clone)]
pub(crate) struct WorkerHandle {
    tx: SyncSender<Command>,
}

impl WorkerHandle {
    pub(crate) fn send(&self, cmd: Command) {
        let _ = self.tx.send(cmd);
    }

    pub(crate) fn call<T: Send + 'static>(
        &self,
        build: impl FnOnce(mpsc::SyncSender<T>) -> Command,
    ) -> Result<T, ServoError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.send(build(reply_tx));
        reply_rx
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| ServoError::WorkerTimeout)
    }
}

// ── Skeleton worker ───────────────────────────────────────────────────────────

/// Placeholder worker state.
///
/// Real version would hold:
///   - `servo: servo::Servo`                            (TODO)
///   - `ctx: Rc<SoftwareRenderingContext>`              (TODO)
///   - `tabs: Vec<(ServoTab, Rc<DelegateState>)>`
///   - `active_idx: Option<usize>`
struct Worker {
    // TODO: servo: servo::Servo
    // TODO: ctx: Rc<servo::rendering_context::SoftwareRenderingContext>
    tabs: Vec<ServoTab>,
    active_idx: Option<usize>,
    next_id: u64,
    /// SharedOsrFrame: kept for when real Servo paints are wired in Phase C.
    #[allow(dead_code)]
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

    /// Open a new tab (skeleton — no real Servo WebView created).
    fn open_tab(&mut self, url_str: &str) -> Result<TabId, ServoError> {
        // TODO: validate URL via url::Url::parse
        // TODO: call WebViewBuilder::new(&self.servo, Rc::clone(&self.ctx))
        //         .delegate(delegate)
        //         .url(url)
        //         .build() → WebView
        // TODO: call webview.resize(PhysicalSize::new(self.width, self.height))
        tracing::info!(
            "servo (skeleton): open_tab {} — libsqlite3-sys dep conflict, real Servo not started",
            url_str
        );
        let id = self.next_tab_id();
        self.tabs.push(ServoTab {
            id,
            url: url_str.to_owned(),
            title: String::new(),
            is_loading: false, // skeleton: never actually loads
        });
        self.active_idx = Some(self.tabs.len() - 1);
        Ok(id)
    }

    fn close_tab(&mut self, id: TabId) -> Result<bool, ServoError> {
        let idx = self
            .tab_index_by_id(id)
            .ok_or(ServoError::TabNotFound(id))?;
        // TODO: drop the WebView handle
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
        Ok(!self.tabs.is_empty())
    }

    fn select_tab(&mut self, id: TabId) {
        if let Some(idx) = self.tab_index_by_id(id) {
            self.active_idx = Some(idx);
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
    }

    fn navigate(&mut self, url_str: &str) -> Result<(), ServoError> {
        // TODO: url::Url::parse(url_str)?
        // TODO: self.active_webview().load(url)
        tracing::info!(
            "servo (skeleton): navigate {} — Unimplemented (dep conflict)",
            url_str
        );
        if let Some(idx) = self.active_idx {
            self.tabs[idx].url = url_str.to_owned();
        }
        Err(ServoError::InitFailed(
            "servo: libsqlite3-sys dep conflict — see Cargo.toml".into(),
        ))
    }

    fn go_back(&self, _n: usize) {
        // TODO: self.active_webview().go_back(n)
        tracing::debug!("servo (skeleton): go_back — Unimplemented (dep conflict)");
    }

    fn go_forward(&self, _n: usize) {
        // TODO: self.active_webview().go_forward(n)
        tracing::debug!("servo (skeleton): go_forward — Unimplemented (dep conflict)");
    }

    fn reload(&self) {
        // TODO: self.active_webview().reload()
        tracing::debug!("servo (skeleton): reload — Unimplemented (dep conflict)");
    }

    fn stop(&self) {
        // TODO: self.active_webview().load(Url::parse("about:blank")?)
        tracing::debug!("servo (skeleton): stop — Unimplemented (dep conflict)");
    }

    fn resize(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
        self.view.width.store(width, Ordering::Relaxed);
        self.view.height.store(height, Ordering::Relaxed);
        // TODO: self.ctx.resize(PhysicalSize::new(width, height))
        // TODO: for each tab webview: wv.resize(size)
        tracing::debug!("servo (skeleton): resize {width}×{height}");
    }

    fn send_input(&self, event: &ServoInputEvent) {
        // TODO: self.active_webview().notify_input_event(real_event)
        tracing::debug!(
            "servo (skeleton): input {} — Unimplemented (dep conflict)",
            event.description
        );
    }

    fn build_snapshot(&self) -> TabsSnapshot {
        let tabs = self.tabs.clone();
        let active = self.active_idx.and_then(|i| tabs.get(i).map(|t| t.id));
        TabsSnapshot { tabs, active }
    }

    /// Skeleton paint: no-op.  Real version would call:
    ///   `wv.paint()` then `self.ctx.read_to_image(rect)` → RGBA → BGRA → frame.
    fn maybe_paint(&self) {
        // TODO: check delegate frame_ready flag
        // TODO: wv.paint()
        // TODO: self.ctx.read_to_image(Box2D::new(0,0, w, h)) → convert RGBA→BGRA
        // TODO: write into self.frame
        // TODO: bump frame.generation
        // TODO: call self.view.wake if set
    }

    fn run(mut self, rx: Receiver<Command>) {
        tracing::info!(
            "servo worker (skeleton): running — real Servo not started due to dep conflict"
        );
        loop {
            let mut quit = false;
            loop {
                match rx.try_recv() {
                    Ok(cmd) => {
                        if self.handle_command(cmd) {
                            quit = true;
                        }
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        quit = true;
                        break;
                    }
                }
            }
            if quit {
                tracing::info!("servo worker: shutting down");
                break;
            }
            // TODO: self.servo.spin_event_loop()
            self.maybe_paint();
            thread::sleep(Duration::from_millis(16));
        }
    }

    fn handle_command(&mut self, cmd: Command) -> bool {
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
            Command::Resize { width, height } => {
                self.resize(width, height);
            }
            Command::SendInput { event } => {
                self.send_input(&event);
            }
            Command::QueryTabs { reply } => {
                let _ = reply.send(self.build_snapshot());
            }
            Command::Shutdown => {
                return true;
            }
        }
        false
    }
}

// ── Spawn helper ──────────────────────────────────────────────────────────────

/// Spawn the Servo worker thread.
///
/// In skeleton mode, starts a no-op worker that drains commands and logs them.
/// Real Servo construction is `// TODO`-commented inside the thread closure.
pub(crate) fn spawn(
    initial_url: &str,
    width: u32,
    height: u32,
    frame: SharedOsrFrame,
    view: SharedOsrViewState,
) -> Result<WorkerHandle, ServoError> {
    let (tx, rx): (SyncSender<Command>, Receiver<Command>) = mpsc::sync_channel(64);
    let initial_url = initial_url.to_owned();

    let handle = WorkerHandle { tx };

    thread::Builder::new()
        .name("buffr-servo-worker".into())
        .spawn(move || {
            tracing::info!("servo worker: starting (skeleton mode — libsqlite3-sys dep conflict)");

            // TODO: SoftwareRenderingContext::new(PhysicalSize::new(width, height))
            // TODO: ServoBuilder::default().build() → servo
            // TODO: WebViewBuilder::new(&servo, Rc::clone(&ctx))
            //         .url(Url::parse(&initial_url)?)
            //         .build() → WebView

            let mut worker = Worker::new(Arc::clone(&frame), Arc::clone(&view), width, height);

            // Open a skeleton tab (records the URL but doesn't load it).
            if let Err(e) = worker.open_tab(&initial_url) {
                tracing::error!("servo: failed to record initial tab: {e}");
            }

            worker.run(rx);
            tracing::info!("servo worker: exited");
        })
        .map_err(|e| ServoError::InitFailed(e.to_string()))?;

    Ok(handle)
}
