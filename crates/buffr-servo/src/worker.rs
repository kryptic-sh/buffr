//! Servo worker thread — owns `servo::Servo`, `WebView` handles, and the OSR
//! frame pipeline.
//!
//! All `servo::Servo` and `servo::WebView` values are `!Send` (they hold
//! `Rc<…>` internally).  They must live and be used exclusively on the worker
//! thread.
//!
//! # State propagation from delegate callbacks
//!
//! `WebViewDelegate` callbacks are called from `servo.spin_event_loop()` on
//! the same worker thread.  We share `Rc<RefCell<TabMeta>>` between each
//! `TabEntry` (held by the worker) and its `BuffrDelegate` instance.  The
//! delegate writes into that cell; the worker reads it after every
//! `spin_event_loop()` call.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread;
use std::time::Duration;

use buffr_engine::{SharedOsrFrame, SharedOsrViewState, TabId, TabSummary};
use dpi::PhysicalSize;
use servo::{
    LoadStatus, RenderingContext, ServoBuilder, SoftwareRenderingContext, WebView, WebViewBuilder,
    WebViewDelegate,
};
// webrender_api::units are re-exported by servo at the root level
use servo::{DeviceIntPoint, DeviceIntRect, DeviceIntSize};
use url::Url;

use crate::error::ServoError;
use crate::input::ServoInputEvent;

// ── Delegate shared state ─────────────────────────────────────────────────────

/// Mutable per-tab metadata written by the delegate callback, read by the
/// worker after `spin_event_loop()`.
#[derive(Default)]
pub(crate) struct TabMeta {
    pub url: String,
    pub title: String,
    pub is_loading: bool,
    /// Set to `true` by `notify_new_frame_ready`; consumed by `maybe_paint`.
    pub frame_ready: bool,
}

// ── WebViewDelegate impl ──────────────────────────────────────────────────────

/// Per-tab delegate.  Holds a shared ref to the tab's `TabMeta` cell.
struct BuffrDelegate {
    meta: Rc<RefCell<TabMeta>>,
}

impl WebViewDelegate for BuffrDelegate {
    fn notify_url_changed(&self, _webview: WebView, url: Url) {
        self.meta.borrow_mut().url = url.to_string();
    }

    fn notify_page_title_changed(&self, _webview: WebView, title: Option<String>) {
        self.meta.borrow_mut().title = title.unwrap_or_default();
    }

    fn notify_load_status_changed(&self, _webview: WebView, status: LoadStatus) {
        self.meta.borrow_mut().is_loading = status == LoadStatus::Started;
    }

    fn notify_new_frame_ready(&self, webview: WebView) {
        self.meta.borrow_mut().frame_ready = true;
        // Paint immediately on the delegate callback so the frame is always
        // up-to-date when the worker reads it.
        webview.paint();
    }

    fn notify_history_changed(&self, _webview: WebView, entries: Vec<Url>, current: usize) {
        if let Some(url) = entries.get(current) {
            self.meta.borrow_mut().url = url.to_string();
        }
    }

    fn notify_closed(&self, _webview: WebView) {
        tracing::debug!("servo: webview closed by page (window.close)");
    }

    fn request_navigation(&self, _webview: WebView, navigation_request: servo::NavigationRequest) {
        // Allow all navigations by default.
        navigation_request.allow();
    }
}

// ── Tab record ────────────────────────────────────────────────────────────────

/// One tab's in-engine state.
pub(crate) struct TabEntry {
    pub id: TabId,
    /// Live Servo WebView handle.
    webview: WebView,
    /// Shared with the delegate — delegate writes here on callbacks.
    meta: Rc<RefCell<TabMeta>>,
}

// ── Commands ──────────────────────────────────────────────────────────────────

/// Commands the engine sends to the worker.
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

/// Lightweight tab summary returned in a `TabsSnapshot`.
#[derive(Clone)]
pub(crate) struct ServoTab {
    pub id: TabId,
    pub url: String,
    pub title: String,
    pub is_loading: bool,
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

// ── Worker ────────────────────────────────────────────────────────────────────

/// The worker owns the real Servo runtime + all WebViews.
///
/// Everything here runs exclusively on the `buffr-servo-worker` thread.
struct Worker {
    servo: servo::Servo,
    /// Software rendering context (CPU readback via `read_to_image`).
    ctx: Rc<SoftwareRenderingContext>,
    tabs: Vec<TabEntry>,
    active_idx: Option<usize>,
    next_id: u64,
    frame: SharedOsrFrame,
    view: SharedOsrViewState,
    width: u32,
    height: u32,
}

impl Worker {
    fn new(
        servo: servo::Servo,
        ctx: Rc<SoftwareRenderingContext>,
        frame: SharedOsrFrame,
        view: SharedOsrViewState,
        width: u32,
        height: u32,
    ) -> Self {
        Worker {
            servo,
            ctx,
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

    fn active_webview(&self) -> Option<&WebView> {
        self.active_idx
            .and_then(|i| self.tabs.get(i))
            .map(|t| &t.webview)
    }

    // ── Tab lifecycle ─────────────────────────────────────────────────────────

    fn open_tab(&mut self, url_str: &str) -> Result<TabId, ServoError> {
        let url = Url::parse(url_str).map_err(|e| ServoError::UrlParse(e.to_string()))?;

        let id = self.next_tab_id();
        let meta = Rc::new(RefCell::new(TabMeta {
            url: url_str.to_owned(),
            ..TabMeta::default()
        }));

        let delegate = Rc::new(BuffrDelegate {
            meta: Rc::clone(&meta),
        });

        let webview = WebViewBuilder::new(
            &self.servo,
            Rc::clone(&self.ctx) as Rc<dyn servo::RenderingContext>,
        )
        .delegate(delegate)
        .url(url)
        .build();

        webview.resize(PhysicalSize::new(self.width, self.height));
        webview.show();

        tracing::info!("servo: opened tab {id:?} → {url_str}");
        self.tabs.push(TabEntry { id, webview, meta });
        self.active_idx = Some(self.tabs.len() - 1);
        Ok(id)
    }

    fn close_tab(&mut self, id: TabId) -> Result<bool, ServoError> {
        let idx = self
            .tab_index_by_id(id)
            .ok_or(ServoError::TabNotFound(id))?;

        // Dropping the WebView signals Servo to clean up the backing pipeline.
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
            self.tabs[idx].webview.focus();
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
        let url = Url::parse(url_str).map_err(|e| ServoError::UrlParse(e.to_string()))?;
        if let Some(wv) = self.active_webview() {
            wv.load(url);
            Ok(())
        } else {
            Err(ServoError::InitFailed("no active tab".into()))
        }
    }

    fn go_back(&self, n: usize) {
        if let Some(wv) = self.active_webview()
            && wv.can_go_back()
        {
            wv.go_back(n);
        }
    }

    fn go_forward(&self, n: usize) {
        if let Some(wv) = self.active_webview()
            && wv.can_go_forward()
        {
            wv.go_forward(n);
        }
    }

    fn reload(&self) {
        if let Some(wv) = self.active_webview() {
            wv.reload();
        }
    }

    fn stop(&self) {
        if let Some(wv) = self.active_webview() {
            // Load about:blank to stop the current load.
            if let Ok(url) = Url::parse("about:blank") {
                wv.load(url);
            }
        }
    }

    fn resize(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
        self.view.width.store(width, Ordering::Relaxed);
        self.view.height.store(height, Ordering::Relaxed);
        let size = PhysicalSize::new(width, height);
        for entry in &self.tabs {
            entry.webview.resize(size);
        }
        tracing::debug!("servo: resize {width}×{height}");
    }

    fn send_input(&self, event: &ServoInputEvent) {
        let Some(wv) = self.active_webview() else {
            return;
        };
        match &event.inner {
            Some(inner) => {
                wv.notify_input_event(inner.clone());
            }
            None => {
                tracing::debug!(
                    "servo: input skipped (no translation): {}",
                    event.description
                );
            }
        }
    }

    // ── OSR paint pipeline ────────────────────────────────────────────────────

    /// Called after `spin_event_loop()`.  If the active tab signalled a new
    /// frame (via `notify_new_frame_ready` → `webview.paint()`), read pixels
    /// from the rendering context and push them into `SharedOsrFrame`.
    ///
    /// `notify_new_frame_ready` already called `webview.paint()`, so the
    /// `SoftwareRenderingContext` has fresh pixels.  We just need to readback.
    fn maybe_readback(&self) {
        let Some(idx) = self.active_idx else { return };
        let Some(entry) = self.tabs.get(idx) else {
            return;
        };

        let frame_ready = {
            let mut meta = entry.meta.borrow_mut();
            std::mem::replace(&mut meta.frame_ready, false)
        };

        if !frame_ready {
            return;
        }

        let rect = DeviceIntRect::from_origin_and_size(
            DeviceIntPoint::new(0, 0),
            DeviceIntSize::new(self.width as i32, self.height as i32),
        );

        let Some(rgba_image) = RenderingContext::read_to_image(self.ctx.as_ref(), rect) else {
            tracing::debug!("servo: read_to_image returned None");
            return;
        };

        // Servo returns RGBA; buffr-engine OSR expects BGRA.
        let mut pixels = rgba_image.into_raw();
        for chunk in pixels.chunks_exact_mut(4) {
            chunk.swap(0, 2); // R ↔ B
        }

        if let Ok(mut guard) = self.frame.lock() {
            if guard.pixels.len() != pixels.len() {
                guard.pixels = pixels;
            } else {
                guard.pixels.copy_from_slice(&pixels);
            }
            guard.width = self.width;
            guard.height = self.height;
            guard.needs_fresh = false;
            guard.generation = guard.generation.wrapping_add(1);
        }

        if let Some(wake) = self.view.wake.get() {
            wake();
        }
    }

    fn build_snapshot(&self) -> TabsSnapshot {
        let tabs: Vec<ServoTab> = self
            .tabs
            .iter()
            .map(|e| {
                let meta = e.meta.borrow();
                ServoTab {
                    id: e.id,
                    url: meta.url.clone(),
                    title: meta.title.clone(),
                    is_loading: meta.is_loading,
                }
            })
            .collect();
        let active = self.active_idx.and_then(|i| tabs.get(i).map(|t| t.id));
        TabsSnapshot { tabs, active }
    }

    // ── Event loop ────────────────────────────────────────────────────────────

    fn run(mut self, rx: Receiver<Command>) {
        tracing::info!("servo worker: running");
        loop {
            let mut quit = false;

            // Drain pending commands.
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

            // Pump Servo's internal event loop (delegate callbacks fire here).
            self.servo.spin_event_loop();

            // Read pixels if the active tab has a new frame.
            self.maybe_readback();

            // 60 fps target: sleep if no animation is pending.
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
/// Constructs `SoftwareRenderingContext`, `ServoBuilder::default().build()`,
/// and opens the initial URL in a new `WebView`.
///
/// # Errors
///
/// Returns `ServoError::InitFailed` if:
/// - `SoftwareRenderingContext::new()` fails (requires EGL/GLX or Mesa softpipe)
/// - `ServoBuilder::build()` panics (Servo may require a display on some hosts)
///
/// If this panics in headless CI, mark callers `#[ignore]`.
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

    // All Servo / WebView state must be created on the worker thread because
    // both types are !Send (they wrap Rc<…> internally).
    thread::Builder::new()
        .name("buffr-servo-worker".into())
        .spawn(move || {
            tracing::info!("servo worker: starting");

            // 1. Create the software rendering context.
            let ctx = match SoftwareRenderingContext::new(PhysicalSize::new(width, height)) {
                Ok(c) => Rc::new(c),
                Err(e) => {
                    // surfman::Error doesn't impl Display; use Debug.
                    tracing::error!("servo: SoftwareRenderingContext::new failed: {e:?}");
                    // Worker exits; WorkerHandle commands will timeout.
                    return;
                }
            };
            if let Err(e) = RenderingContext::make_current(ctx.as_ref()) {
                tracing::error!("servo: make_current failed: {e:?}");
                return;
            }

            // 2. Build the Servo instance.
            let servo = ServoBuilder::default().build();

            // 3. Create worker struct.
            let mut worker = Worker::new(
                servo,
                ctx,
                Arc::clone(&frame),
                Arc::clone(&view),
                width,
                height,
            );

            // 4. Open initial tab.
            match worker.open_tab(&initial_url) {
                Ok(id) => tracing::info!("servo worker: initial tab {id:?} opened"),
                Err(e) => tracing::error!("servo worker: failed to open initial tab: {e}"),
            }

            worker.run(rx);
            tracing::info!("servo worker: exited");
        })
        .map_err(|e| ServoError::InitFailed(e.to_string()))?;

    Ok(handle)
}
