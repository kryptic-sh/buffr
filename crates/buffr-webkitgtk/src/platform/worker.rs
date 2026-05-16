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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use buffr_core::DownloadNoticeQueue;
use buffr_core::find::FindResultSink;
use buffr_core::hint::HintEventSink;
use buffr_downloads::Downloads;
use buffr_engine::{SharedOsrFrame, SharedOsrViewState, TabId, TabSummary};
use buffr_history::History;

use super::error::WebKitGtkError;
use super::input::{GtkImeEvent, GtkInputEvent, GtkMouseButton, GtkMouseKind};
use super::input_js::{
    ime_cancel_js, ime_commit_js, ime_set_composition_js, key_js, mouse_click_js, mouse_leave_js,
    mouse_move_js, wheel_js,
};
use super::osr::{paint_blank, request_snapshot};
use super::runtime::{OsrHandles, TabEntry, TabSinks};

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
    /// Cached result of the 500 ms audio-activity poll.
    ///
    /// Written by `glib::timeout_add_local` on the GTK thread; read from any
    /// thread by `BrowserEngine::any_audio_active`.  `Relaxed` ordering is
    /// sufficient — no synchronisation is required between the writer and the
    /// reader.
    pub audio_active: Arc<AtomicBool>,
    /// Cached loading state of the **active** tab.
    ///
    /// Written by the `load-changed` GTK signal whenever the active tab's
    /// loading status changes, and by `sync_active_idx` whenever the active
    /// tab switches.  Read from any thread by
    /// `BrowserEngine::is_loading` without locking the `Mutex<EngineState>`.
    /// `Relaxed` ordering is sufficient — same rationale as `audio_active`.
    pub loading_active: Arc<AtomicBool>,
    /// `true` while the engine is sleeping (OSR paint suppressed).
    ///
    /// Set by `BrowserEngine::osr_sleep`; read by the 250 ms snapshot tick
    /// to skip `request_snapshot` when sleeping.  `Relaxed` ordering is
    /// sufficient — no cross-thread synchronisation needed beyond visibility.
    pub osr_sleeping: Arc<AtomicBool>,
    /// Pending favicon URL updates, drained by `drain_favicon_updates`.
    ///
    /// Written by the `connect_favicon_notify` GTK signal; drained from any
    /// thread by `BrowserEngine::drain_favicon_updates` via a
    /// `Command::DrainFaviconUpdates` round-trip to the GTK thread.
    pub favicon_updates: Arc<Mutex<Vec<buffr_engine::FaviconUpdate>>>,
    /// Last cursor change from `mouse-target-changed`.
    ///
    /// Written on the GTK thread by `connect_mouse_target_changed`; read from
    /// any thread by `BrowserEngine::take_cursor_change` via
    /// `Command::TakeCursorChange`.  `Option<(i32, u32)>` matches the
    /// `BrowserEngine::take_cursor_change` return type directly.
    pub cursor_change: Arc<Mutex<Option<(i32, u32)>>>,
}

impl EngineState {
    pub(crate) fn new(width: u32, height: u32) -> Self {
        EngineState {
            tabs: Vec::new(),
            active_idx: None,
            next_id: 1,
            width,
            height,
            audio_active: Arc::new(AtomicBool::new(false)),
            loading_active: Arc::new(AtomicBool::new(false)),
            osr_sleeping: Arc::new(AtomicBool::new(false)),
            favicon_updates: Arc::new(Mutex::new(Vec::new())),
            cursor_change: Arc::new(Mutex::new(None)),
        }
    }

    /// Sync `loading_active` from the active tab's `is_loading` field.
    ///
    /// Call whenever `active_idx` changes or any tab's `is_loading` is updated.
    pub(crate) fn sync_loading_active(&self) {
        let loading = self
            .active_idx
            .and_then(|i| self.tabs.get(i))
            .map(|t| t.is_loading)
            .unwrap_or(false);
        self.loading_active.store(loading, Ordering::Relaxed);
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
    /// Current zoom level. Updated from the GTK thread after every
    /// `webview.set_zoom_level(...)` call; read by `active_zoom_level()`.
    pub zoom: f64,
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
    /// Set the active tab's zoom level. Clamped to [0.25, 5.0].
    SetZoom(f64),
    /// Evaluate `code` in the active tab's JS context (fire-and-forget).
    ///
    /// Dispatched to the GTK main thread and calls
    /// `webkit6::prelude::WebViewExt::evaluate_javascript`. The callback is
    /// a no-op — matching the trait's fire-and-forget contract.
    EvalJs {
        code: String,
    },
    /// Start an in-page find. `forward = false` searches backwards.
    StartFind {
        query: String,
        forward: bool,
    },
    /// Cancel the active find session.
    StopFind,
    Resize {
        width: u32,
        height: u32,
    },
    ForcePaint,
    /// Dispatch an input event to the active WebView via JS injection.
    ///
    /// Fire-and-forget: no reply channel. Input events return `()` on the
    /// `BrowserEngine` trait surface.
    ///
    /// Phase C: dispatched via `WebView::evaluate_javascript` DOM event
    /// injection. Phase D may use native `gdk4::Event` dispatch once safe
    /// constructors are available (gdk4 >= 0.12).
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
    /// Open the WebInspector for the tab with the given id.
    ///
    /// Calls `webkit6::WebInspector::show()` on the inspector associated with
    /// the tab's `WebView`. Requires `enable-developer-extras = true` on the
    /// `WebView`'s `WebkitSettings` (set during view creation in `TabEntry::new`).
    OpenDevtools {
        id: TabId,
        reply: mpsc::SyncSender<Result<(), WebKitGtkError>>,
    },
    /// Put the active WebView to sleep or wake it up.
    ///
    /// When `sleep = true`, mutes the WebView and stops snapshot ticks.
    /// When `sleep = false`, unmutes and resumes ticks.
    SetOsrSleep {
        sleep: bool,
    },
    /// Drain all pending favicon updates from the internal queue.
    ///
    /// Returns the accumulated `Vec<FaviconUpdate>` collected from
    /// `connect_favicon_notify` callbacks since the last drain.
    DrainFaviconUpdates {
        reply: mpsc::SyncSender<Vec<buffr_engine::FaviconUpdate>>,
    },
    /// Take the last cursor change, if any.
    ///
    /// Returns `Some((browser_id, raw_kind))` when a new cursor has arrived
    /// since the last call via `mouse-target-changed`, `None` otherwise.
    TakeCursorChange {
        reply: mpsc::SyncSender<Option<(i32, u32)>>,
    },
    /// Write `text` to the system clipboard via GDK4.
    ///
    /// Fire-and-forget: no reply channel. Called on the GTK thread where
    /// `gdk::Display::default()` is valid.
    ClipboardSetText {
        text: String,
    },
    /// Trigger a WebKitGTK-managed download for `url`.
    ///
    /// Calls `WebViewExt::download_uri` which hands the download off to the
    /// WebKit download manager.  More robust than the JS `<a>` click trick for
    /// direct-resource URLs and cross-origin assets.
    DownloadUri {
        url: String,
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
    /// Forwarded from the engine so every new `WebView` can register the
    /// `buffrHint` script-message handler and write events into it.
    hint_sink: HintEventSink,
    /// Apps-layer sinks forwarded to each new `TabEntry`.
    history: Option<Arc<History>>,
    /// Kept alive so the `NetworkSession::download-started` closure can
    /// reference the stores. The fields are not read directly on the
    /// GTK thread after construction — the closures capture clones.
    #[allow(dead_code)]
    downloads: Option<Arc<Downloads>>,
    #[allow(dead_code)]
    notice_queue: Option<DownloadNoticeQueue>,
    find_sink: FindResultSink,
}

impl GtkRuntime {
    #[allow(clippy::too_many_arguments)]
    fn new(
        frame: SharedOsrFrame,
        view: SharedOsrViewState,
        engine_state: Arc<Mutex<EngineState>>,
        hint_sink: HintEventSink,
        history: Option<Arc<History>>,
        downloads: Option<Arc<Downloads>>,
        notice_queue: Option<DownloadNoticeQueue>,
        find_sink: FindResultSink,
    ) -> Self {
        GtkRuntime {
            tabs: Vec::new(),
            active_idx: None,
            frame,
            view,
            engine_state,
            snapshot_in_flight: std::rc::Rc::new(std::cell::Cell::new(false)),
            hint_sink,
            history,
            downloads,
            notice_queue,
            find_sink,
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
            Arc::clone(&self.hint_sink),
            TabSinks {
                history: self.history.clone(),
                find_sink: self.find_sink.clone(),
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

    fn set_zoom(&mut self, level: f64) {
        use webkit6::prelude::WebViewExt;
        let clamped = level.clamp(0.25, 5.0);
        let Some(active_idx) = self.active_idx else {
            return;
        };
        let id = self.tabs[active_idx].id;
        self.tabs[active_idx].web_view.set_zoom_level(clamped);
        // Mirror the clamped value into the shared EngineState so
        // active_zoom_level() reads it without round-tripping the
        // GTK thread.
        if let Ok(mut guard) = self.engine_state.lock()
            && let Some(t) = guard.tabs.iter_mut().find(|t| t.id == id)
        {
            t.zoom = clamped;
        }
    }

    /// Evaluate `code` in the active tab's JS context (fire-and-forget).
    fn eval_js(&self, code: &str) {
        use webkit6::prelude::WebViewExt;
        if let Some(tab) = self.active_tab() {
            tab.web_view.evaluate_javascript(
                code,
                None,
                None,
                None::<&webkit6::gio::Cancellable>,
                |_| {},
            );
        } else {
            tracing::debug!("webkitgtk worker: eval_js — no active tab, dropping");
        }
    }

    fn start_find(&mut self, query: &str, forward: bool) {
        use webkit6::prelude::WebViewExt;
        const CASE_INSENSITIVE: u32 = 1;
        const BACKWARDS: u32 = 8;
        let Some(active_idx) = self.active_idx else {
            return;
        };
        let Some(fc) = self.tabs[active_idx].web_view.find_controller() else {
            tracing::warn!("webkitgtk: no FindController on active WebView");
            return;
        };
        let opts = if forward {
            CASE_INSENSITIVE
        } else {
            CASE_INSENSITIVE | BACKWARDS
        };
        fc.search(query, opts, u32::MAX);
    }

    fn stop_find(&mut self) {
        use webkit6::prelude::WebViewExt;
        let Some(active_idx) = self.active_idx else {
            return;
        };
        if let Some(fc) = self.tabs[active_idx].web_view.find_controller() {
            fc.search_finish();
        }
    }

    fn open_devtools(&self, id: TabId) -> Result<(), WebKitGtkError> {
        use webkit6::prelude::WebViewExt;
        let tab = self
            .tabs
            .iter()
            .find(|t| t.id == id)
            .ok_or(WebKitGtkError::TabNotFound(id))?;
        match tab.web_view.inspector() {
            Some(inspector) => {
                inspector.show();
                tracing::debug!("webkitgtk: open_devtools — inspector shown for tab {id:?}");
                Ok(())
            }
            None => {
                tracing::debug!(
                    "webkitgtk: open_devtools — no inspector for tab {id:?} \
                     (enable-developer-extras not set?)"
                );
                Ok(())
            }
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

    /// Write active_idx into the shared EngineState and sync `loading_active`
    /// from the newly-active tab's current `is_loading` value.
    fn sync_active_idx(&self) {
        if let Ok(mut st) = self.engine_state.lock() {
            st.active_idx = self.active_idx;
            st.sync_loading_active();
        }
    }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Map a `GtkMouseButton` to the JS `MouseEvent.button` index.
fn gtk_button_to_js(btn: &GtkMouseButton) -> u32 {
    match btn {
        GtkMouseButton::Left => 0,
        GtkMouseButton::Middle => 1,
        GtkMouseButton::Right => 2,
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
        Command::SetZoom(level) => {
            rt.set_zoom(level);
        }
        Command::EvalJs { code } => {
            rt.eval_js(&code);
        }
        Command::StartFind { query, forward } => {
            rt.start_find(&query, forward);
        }
        Command::StopFind => {
            rt.stop_find();
        }
        Command::Resize { width, height } => {
            rt.resize(width, height);
        }
        Command::ForcePaint => {
            rt.paint();
        }
        Command::SendInput(event) => {
            // Phase C: dispatch via JS injection through evaluate_javascript.
            //
            // WebKitGTK / gdk4 0.11 has no safe Rust constructors for synthetic
            // GdkEvent, so we inject DOM events through JS evaluation instead.
            // This covers ~80 % of headless automation cases. Native default
            // actions (Tab focus traversal, form submit) require Phase D with
            // gdk4 >= 0.12 synthetic event constructors.
            //
            // evaluate_javascript is async; the callback fires on this GTK
            // main thread. We ignore the return value — fire-and-forget.
            let js: Option<String> = match &event {
                GtkInputEvent::Key(ev) => {
                    // Map VK code to JS event type based on description context.
                    // NeutralKeyEvent.kind is encoded in the description by
                    // neutral_key_to_gtk; we emit keydown for all key events
                    // to cover the most common case. Char events are not
                    // separately dispatched — keydown with the correct key
                    // field is sufficient for most JS input handlers.
                    let snippet = key_js(ev, "keydown");
                    if snippet.is_none() {
                        tracing::debug!(
                            "webkitgtk worker: unknown VK {} ('{}') — skipping JS key dispatch",
                            ev.windows_key_code,
                            ev.description,
                        );
                    } else {
                        tracing::debug!(
                            "webkitgtk worker: JS key dispatch vk={} ({})",
                            ev.windows_key_code,
                            ev.description,
                        );
                    }
                    snippet
                }
                GtkInputEvent::Mouse(ev) => match &ev.kind {
                    GtkMouseKind::Move => Some(mouse_move_js(ev)),
                    GtkMouseKind::ButtonPress(btn) => {
                        let b = gtk_button_to_js(btn);
                        tracing::debug!(
                            "webkitgtk worker: JS mousedown ({:.0},{:.0}) btn={b}",
                            ev.x,
                            ev.y,
                        );
                        Some(mouse_click_js(ev, b, false))
                    }
                    GtkMouseKind::ButtonRelease(btn) => {
                        let b = gtk_button_to_js(btn);
                        tracing::debug!(
                            "webkitgtk worker: JS mouseup+click ({:.0},{:.0}) btn={b}",
                            ev.x,
                            ev.y,
                        );
                        Some(mouse_click_js(ev, b, true))
                    }
                    GtkMouseKind::Leave => {
                        tracing::debug!("webkitgtk worker: JS mouseleave");
                        Some(mouse_leave_js())
                    }
                    GtkMouseKind::Scroll { .. } => {
                        // Scroll is now routed via GtkInputEvent::Wheel;
                        // this variant is a structural leftover.
                        tracing::debug!("webkitgtk worker: Scroll variant (no-op, use Wheel)");
                        None
                    }
                },
                GtkInputEvent::Wheel(ev) => {
                    tracing::debug!(
                        "webkitgtk worker: JS wheel ({:.0},{:.0}) dx={:.0} dy={:.0}",
                        ev.x,
                        ev.y,
                        ev.delta_x,
                        ev.delta_y,
                    );
                    Some(wheel_js(ev))
                }
                GtkInputEvent::Ime(ev) => {
                    // IME composition / commit / cancel via JS CompositionEvent
                    // injection.  Uses `compositionupdate`, `compositionend`, and
                    // `document.execCommand('insertText')` to approximate what a
                    // real OS-level IME would deliver to the browser's responder
                    // chain.  Full fidelity requires native GDK4 IME support
                    // (Phase D) — this is the headless approximation.
                    match ev {
                        GtkImeEvent::Preedit { text, cursor } => {
                            tracing::debug!(
                                text = %text,
                                ?cursor,
                                "webkitgtk worker: JS ime_set_composition"
                            );
                            Some(ime_set_composition_js(text, *cursor))
                        }
                        GtkImeEvent::Commit { text } => {
                            tracing::debug!(
                                text = %text,
                                "webkitgtk worker: JS ime_commit"
                            );
                            Some(ime_commit_js(text))
                        }
                        GtkImeEvent::Cancel => {
                            tracing::debug!("webkitgtk worker: JS ime_cancel");
                            Some(ime_cancel_js())
                        }
                    }
                }
            };

            if let Some(js) = js {
                if let Some(tab) = rt.active_tab() {
                    use webkit6::prelude::WebViewExt;
                    tab.web_view.evaluate_javascript(
                        &js,
                        None,
                        None,
                        None::<&webkit6::gio::Cancellable>,
                        |result| {
                            if let Err(e) = result {
                                tracing::debug!("webkitgtk worker: JS dispatch error: {e}");
                            }
                        },
                    );
                } else {
                    tracing::debug!("webkitgtk worker: no active tab — dropping input JS");
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
        Command::OpenDevtools { id, reply } => {
            let _ = reply.send(rt.open_devtools(id));
        }
        Command::SetOsrSleep { sleep } => {
            use webkit6::prelude::WebViewExt;
            if let Ok(st) = rt.engine_state.lock() {
                st.osr_sleeping.store(sleep, Ordering::Relaxed);
            }
            // Mute/unmute audio and adjust WebView priority when sleeping.
            if let Some(tab) = rt.active_tab() {
                tab.web_view.set_is_muted(sleep);
                tracing::debug!("webkitgtk worker: osr_sleep({sleep}) — is_muted={sleep}");
            }
        }
        Command::DrainFaviconUpdates { reply } => {
            let updates = rt
                .engine_state
                .lock()
                .map(|st| {
                    let mut v = st.favicon_updates.lock().unwrap_or_else(|e| e.into_inner());
                    std::mem::take(&mut *v)
                })
                .unwrap_or_default();
            let _ = reply.send(updates);
        }
        Command::TakeCursorChange { reply } => {
            let change = rt
                .engine_state
                .lock()
                .map(|st| {
                    let mut v = st.cursor_change.lock().unwrap_or_else(|e| e.into_inner());
                    v.take()
                })
                .unwrap_or(None);
            let _ = reply.send(change);
        }
        Command::ClipboardSetText { text } => {
            // Must run on the GTK thread — `gdk::Display::default()` is only
            // valid here.
            use gtk4::gdk::prelude::DisplayExt;
            if let Some(display) = gtk4::gdk::Display::default() {
                display.clipboard().set_text(&text);
                tracing::debug!(
                    "webkitgtk worker: clipboard_set_text ({} bytes)",
                    text.len()
                );
            } else {
                tracing::warn!("webkitgtk worker: clipboard_set_text — no GDK display");
            }
        }
        Command::DownloadUri { url } => {
            use webkit6::prelude::WebViewExt;
            if let Some(tab) = rt.active_tab() {
                tab.web_view.download_uri(&url);
                tracing::debug!("webkitgtk worker: download_uri {url}");
            } else {
                tracing::debug!("webkitgtk worker: download_uri — no active tab");
            }
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

#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn(
    initial_url: &str,
    _width: u32,
    _height: u32,
    frame: SharedOsrFrame,
    view: SharedOsrViewState,
    engine_state: Arc<Mutex<EngineState>>,
    hint_sink: HintEventSink,
    history: Option<Arc<History>>,
    downloads: Option<Arc<Downloads>>,
    notice_queue: Option<DownloadNoticeQueue>,
    find_sink: FindResultSink,
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
                Arc::clone(&hint_sink),
                history,
                downloads.clone(),
                notice_queue.clone(),
                find_sink.clone(),
            );

            // ── NetworkSession download-started signal ────────────────────
            //
            // Connect to the default NetworkSession's `download-started`
            // signal to record download lifecycle events in the shared
            // `Downloads` store and push notices to `notice_queue`.
            //
            // The signal fires for every download on any WebView that uses
            // the default session. We assign a synthetic `cef_id` via a
            // per-worker monotonic counter so the `Downloads` store (which
            // uses `cef_id` as a natural primary key) stays happy.
            if downloads.is_some() || notice_queue.is_some() {
                if let Some(ns) = webkit6::NetworkSession::default() {
                    let dl_store = downloads.clone();
                    let nq = notice_queue.clone();
                    // We need a shared counter inside the closure. Use an Arc<Mutex>.
                    let dl_counter = Arc::new(Mutex::new(1u32));
                    ns.connect_download_started(move |_session, dl| {
                        let url = dl
                            .request()
                            .and_then(|r| r.uri())
                            .map(|s| s.to_string())
                            .unwrap_or_default();
                        let suggested = dl
                            .destination()
                            .map(|s| {
                                std::path::Path::new(s.as_str())
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                    .unwrap_or(s.as_ref())
                                    .to_owned()
                            })
                            .unwrap_or_else(|| "download".to_owned());

                        let cef_id = {
                            let mut ctr = dl_counter.lock().unwrap_or_else(|e| e.into_inner());
                            let id = *ctr;
                            *ctr = ctr.wrapping_add(1);
                            id
                        };

                        tracing::debug!(url, cef_id, "webkitgtk: download-started");

                        if let Some(store) = &dl_store {
                            match store.record_started(cef_id, &url, &suggested, None, None) {
                                Ok(dl_id) => {
                                    // Push Started notice.
                                    if let Some(nq) = &nq {
                                        buffr_core::download_notice::push(
                                            nq,
                                            buffr_core::DownloadNotice {
                                                kind: buffr_core::DownloadNoticeKind::Started,
                                                filename: suggested.clone(),
                                                path: String::new(),
                                                created_at: std::time::Instant::now(),
                                            },
                                        );
                                    }

                                    // Wire finished/failed signals.
                                    let store_fin = Arc::clone(store);
                                    let nq_fin = nq.clone();
                                    let name_fin = suggested.clone();
                                    dl.connect_finished(move |finished_dl| {
                                        let dest = finished_dl
                                            .destination()
                                            .map(|s| s.to_string())
                                            .unwrap_or_default();
                                        let path = std::path::PathBuf::from(&dest);
                                        if let Err(e) = store_fin.record_completed(dl_id, &path) {
                                            tracing::warn!(error = %e, "webkitgtk: record_completed failed");
                                        }
                                        tracing::debug!(dl_id = dl_id.0, dest, "webkitgtk: download finished");
                                        if let Some(nq) = &nq_fin {
                                            buffr_core::download_notice::push(
                                                nq,
                                                buffr_core::DownloadNotice {
                                                    kind: buffr_core::DownloadNoticeKind::Completed,
                                                    filename: name_fin.clone(),
                                                    path: dest,
                                                    created_at: std::time::Instant::now(),
                                                },
                                            );
                                        }
                                    });

                                    let store_fail = Arc::clone(store);
                                    let nq_fail = nq.clone();
                                    let name_fail = suggested;
                                    dl.connect_failed(move |_dl, err| {
                                        let reason = err.message().to_string();
                                        if let Err(e) = store_fail.record_failed(dl_id, &reason) {
                                            tracing::warn!(error = %e, "webkitgtk: record_failed failed");
                                        }
                                        tracing::debug!(dl_id = dl_id.0, reason, "webkitgtk: download failed");
                                        if let Some(nq) = &nq_fail {
                                            buffr_core::download_notice::push(
                                                nq,
                                                buffr_core::DownloadNotice {
                                                    kind: buffr_core::DownloadNoticeKind::Failed,
                                                    filename: name_fail.clone(),
                                                    path: String::new(),
                                                    created_at: std::time::Instant::now(),
                                                },
                                            );
                                        }
                                    });
                                }
                                Err(e) => {
                                    tracing::warn!(error = %e, "webkitgtk: record_started failed");
                                }
                            }
                        } else if let Some(nq) = &nq {
                            // No Downloads store, just push the notice.
                            buffr_core::download_notice::push(
                                nq,
                                buffr_core::DownloadNotice {
                                    kind: buffr_core::DownloadNoticeKind::Started,
                                    filename: suggested,
                                    path: String::new(),
                                    created_at: std::time::Instant::now(),
                                },
                            );
                        }
                    });
                } else {
                    tracing::debug!(
                        "webkitgtk: no default NetworkSession — download tracking unavailable"
                    );
                }
            }

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
            // Skipped entirely when `osr_sleeping` is set (Gap 3).
            {
                let rt_rc_snap = Rc::clone(&runtime_rc);
                let engine_state_snap = Arc::clone(&engine_state);
                glib::timeout_add_local(Duration::from_millis(250), move || {
                    // Skip paint while sleeping — avoids stale snapshots and
                    // reduces CPU load when the view is hidden.
                    let sleeping = engine_state_snap
                        .lock()
                        .map(|st| st.osr_sleeping.load(Ordering::Relaxed))
                        .unwrap_or(false);
                    if !sleeping {
                        let rt = rt_rc_snap.borrow();
                        rt.paint();
                    }
                    glib::ControlFlow::Continue
                });
            }

            // ── Audio-activity poll tick (500 ms) ─────────────────────────
            //
            // Walks all open WebViews on the GTK thread and checks
            // `WebViewExt::is_playing_audio()`.  The ORed result is stored in
            // `EngineState::audio_active` (an `Arc<AtomicBool>`) so the engine
            // thread can read it from any thread without blocking.
            {
                let rt_rc_audio = Rc::clone(&runtime_rc);
                glib::timeout_add_local(Duration::from_millis(500), move || {
                    use webkit6::prelude::WebViewExt;
                    let rt = rt_rc_audio.borrow();
                    let any_audio = rt.tabs.iter().any(|t| t.web_view.is_playing_audio());
                    if let Ok(st) = rt.engine_state.lock() {
                        st.audio_active.store(any_audio, Ordering::Relaxed);
                    }
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
