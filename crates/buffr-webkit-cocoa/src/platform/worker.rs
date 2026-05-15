//! WebKit Cocoa worker: runs WKWebView operations on the macOS main queue.
//!
//! # Architecture
//!
//! WKWebView **must** live on the main thread (AppKit run loop). Buffr's
//! `BrowserEngine` methods are called from any thread.
//!
//! Solution: **dedicated `std::thread`** that receives `Command` values from an
//! `mpsc` channel and dispatches closures to the GCD main queue for each.
//!
//! The main queue closures own an `Arc<Mutex<RuntimeState>>` and carry a
//! `SyncSender<T>` for synchronous-reply commands.
//!
//! # API verification notes (grepped from registry crate source)
//!
//! ## dispatch2 0.3.1 — DispatchQueue
//!
//! `Queue` is a deprecated type alias for `DispatchQueue`.
//! Confirmed: dispatch2-0.3.1/src/lib.rs:123-125.
//!
//! **Correct type to use:** `dispatch2::DispatchQueue`.
//!
//! `DispatchQueue::main()` returns `&'static DispatchQueue`.
//! Confirmed: dispatch2-0.3.1/src/queue.rs:108-114.
//!
//! `DispatchQueue::exec_async<F: FnOnce() + Send + 'static>(f: F)`
//! accepts a `move` closure that is `Send + 'static`. No `block2` required.
//! Confirmed: dispatch2-0.3.1/src/queue.rs:134-143.
//!
//! ## RuntimeState Send requirement
//!
//! GCD's `exec_async` requires the closure to be `Send`. `RuntimeState`
//! contains `Arc<Mutex<…>>` which is `Send`. We use `Arc<Mutex<RuntimeState>>`
//! (not `Rc<RefCell<…>>`) so the closures are `Send`.
//!
//! The GCD main queue is serial, so `Mutex` guard contention is impossible
//! in practice (only one closure runs at a time). The `Mutex` cost is the
//! price of `Send`-safety across the thread boundary.

#[cfg(target_os = "macos")]
pub(crate) use macos::*;

#[cfg(target_os = "macos")]
pub(crate) mod macos {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, mpsc};
    use std::thread;
    use std::time::Duration;

    use buffr_engine::{SharedOsrFrame, SharedOsrViewState, TabId, TabSummary};

    use dispatch2::{DispatchQueue, DispatchTime};

    use super::super::error::WebKitCocoaError;
    use super::super::runtime::macos::{TabEntry, TabState};

    // ── EngineState (thread-safe snapshot) ────────────────────────────────────

    /// Shared tab snapshot. Updated by main-thread delegate callbacks;
    /// read from any thread by the engine.
    pub(crate) struct EngineState {
        /// Snapshot of open tabs in strip order.
        pub tabs: Vec<TabState>,
        /// Index of the active tab.
        pub active_idx: Option<usize>,
        /// Cached result of the 500 ms audio-activity poll.
        ///
        /// Written by `dispatch_main_async` on the GCD main queue; read from
        /// any thread by `BrowserEngine::any_audio_active`.  `Relaxed` ordering
        /// is sufficient — no synchronisation is required between writer and
        /// reader.
        pub audio_active: Arc<AtomicBool>,
        /// Cached loading state of the **active** tab.
        ///
        /// Written by `WKNavigationDelegate` callbacks (on the GCD main queue)
        /// and by the worker's active-tab switch helpers.  Read lock-free from
        /// any thread by `BrowserEngine::is_loading`.
        /// `Relaxed` ordering is sufficient — same rationale as `audio_active`.
        pub loading_active: Arc<AtomicBool>,
    }

    impl EngineState {
        pub(crate) fn new() -> Self {
            EngineState {
                tabs: Vec::new(),
                active_idx: None,
                audio_active: Arc::new(AtomicBool::new(false)),
                loading_active: Arc::new(AtomicBool::new(false)),
            }
        }

        #[allow(dead_code)] // Used in Phase D for popup/hint routing.
        pub(crate) fn active_id(&self) -> Option<TabId> {
            self.active_idx.and_then(|i| self.tabs.get(i)).map(|t| t.id)
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

        pub(crate) fn summaries(&self) -> Vec<TabSummary> {
            self.tabs
                .iter()
                .map(|t| TabSummary {
                    id: t.id,
                    browser_id: 0,
                    title: t.title.clone(),
                    url: t.url.clone(),
                    progress: if t.is_loading { 0.5 } else { 1.0 },
                    is_loading: t.is_loading,
                    pinned: false,
                    private: false,
                })
                .collect()
        }
    }

    // ── MainQueueBox ──────────────────────────────────────────────────────────

    /// A `Send`-asserting wrapper for values that must only be accessed on the
    /// GCD main queue.
    ///
    /// # Safety
    ///
    /// The wrapped value (`T`) is `!Send` (e.g. contains ObjC objects with
    /// `#[thread_kind = MainThreadOnly]`). We assert `Send` here because every
    /// consumer of `MainQueueBox` passes it into a `dispatch_main_async` closure
    /// that runs exclusively on the GCD main queue — a single serial queue.
    ///
    /// The `Arc<Mutex<MainQueueBox<T>>>` pattern means the value is always
    /// accessed under the mutex, and since the GCD main queue is serial, the
    /// mutex is never actually contended. The `Mutex` wrapper is kept because
    /// `Arc<T>` requires `T: Sync` for the `Arc` itself to be `Send`; wrapping in
    /// `Mutex` satisfies that without requiring `T: Sync`.
    struct MainQueueBox<T>(T);

    // SAFETY: `T` is only ever accessed from closures dispatched via
    // `dispatch_main_async`, which run on the single serial GCD main queue.
    // No other thread can hold a reference to the inner value simultaneously.
    unsafe impl<T> Send for MainQueueBox<T> {}
    // SAFETY: same guarantee — serial main queue prevents concurrent access.
    unsafe impl<T> Sync for MainQueueBox<T> {}

    // ── RuntimeState (main-thread only) ───────────────────────────────────────

    /// Mutable per-tab WKWebView table. Accessed only from the GCD main queue.
    ///
    /// Stored inside `Arc<Mutex<MainQueueBox<RuntimeState>>>` so that closures
    /// dispatched via `dispatch_main_async` (which require `Send + 'static`) can
    /// capture an `Arc` clone. `MainQueueBox` asserts `Send + Sync` by safety
    /// contract: all access is serialised on the GCD main queue.
    struct RuntimeState {
        tabs: Vec<TabEntry>,
        active_idx: Option<usize>,
        next_id: u64,
        frame: SharedOsrFrame,
        view: SharedOsrViewState,
        engine_state: Arc<Mutex<EngineState>>,
    }

    impl RuntimeState {
        fn new(
            frame: SharedOsrFrame,
            view: SharedOsrViewState,
            engine_state: Arc<Mutex<EngineState>>,
        ) -> Self {
            RuntimeState {
                tabs: Vec::new(),
                active_idx: None,
                next_id: 1,
                frame,
                view,
                engine_state,
            }
        }

        fn next_id(&mut self) -> TabId {
            let id = TabId(self.next_id);
            self.next_id += 1;
            id
        }

        fn tab_index_by_id(&self, id: TabId) -> Option<usize> {
            self.tabs.iter().position(|t| t.id == id)
        }

        fn snapshot_to_engine_state(&self) {
            if let Ok(mut st) = self.engine_state.lock() {
                st.tabs = self
                    .tabs
                    .iter()
                    .map(|t| TabState {
                        id: t.id,
                        url: t.url.clone(),
                        title: t.title.clone(),
                        is_loading: t.is_loading,
                        can_go_back: t.can_go_back(),
                        can_go_forward: t.can_go_forward(),
                        zoom: t.zoom,
                    })
                    .collect();
                st.active_idx = self.active_idx;
                // Keep loading_active current whenever the tab list or
                // active-tab index changes.
                st.sync_loading_active();
            }
        }

        fn request_active_snapshot(&self) {
            if let Some(idx) = self.active_idx {
                if let Some(tab) = self.tabs.get(idx) {
                    tab.request_snapshot(Arc::clone(&self.frame), Arc::clone(&self.view));
                    return;
                }
            }
            // No active tab — paint blank.
            super::super::osr::paint_blank(&self.frame, &self.view);
        }

        fn open_tab(&mut self, url: &str) -> Result<TabId, WebKitCocoaError> {
            let id = self.next_id();
            let w = self.view.width.load(std::sync::atomic::Ordering::Relaxed);
            let h = self.view.height.load(std::sync::atomic::Ordering::Relaxed);
            let entry = TabEntry::open(id, url, w, h, Arc::clone(&self.engine_state))?;
            self.tabs.push(entry);
            self.active_idx = Some(self.tabs.len() - 1);
            self.snapshot_to_engine_state();
            self.request_active_snapshot();
            Ok(id)
        }

        fn close_tab(&mut self, id: TabId) -> Result<bool, WebKitCocoaError> {
            let idx = self
                .tab_index_by_id(id)
                .ok_or(WebKitCocoaError::TabNotFound(id))?;
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
            self.snapshot_to_engine_state();
            self.request_active_snapshot();
            Ok(!self.tabs.is_empty())
        }

        fn select_tab(&mut self, id: TabId) {
            if let Some(idx) = self.tab_index_by_id(id) {
                self.active_idx = Some(idx);
                self.snapshot_to_engine_state();
                self.request_active_snapshot();
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
            self.snapshot_to_engine_state();
            self.request_active_snapshot();
        }

        fn navigate(&mut self, url: &str) {
            if let Some(idx) = self.active_idx {
                if let Some(tab) = self.tabs.get(idx) {
                    tab.navigate(url);
                }
            }
        }

        fn go_back(&mut self) {
            if let Some(idx) = self.active_idx {
                if let Some(tab) = self.tabs.get(idx) {
                    tab.go_back();
                }
            }
        }

        fn go_forward(&mut self) {
            if let Some(idx) = self.active_idx {
                if let Some(tab) = self.tabs.get(idx) {
                    tab.go_forward();
                }
            }
        }

        fn reload(&mut self) {
            if let Some(idx) = self.active_idx {
                if let Some(tab) = self.tabs.get(idx) {
                    tab.reload();
                }
            }
        }

        fn stop(&mut self) {
            if let Some(idx) = self.active_idx {
                if let Some(tab) = self.tabs.get(idx) {
                    tab.stop();
                }
            }
        }

        fn resize(&mut self, width: u32, height: u32) {
            use std::sync::atomic::Ordering;
            self.view.width.store(width, Ordering::Relaxed);
            self.view.height.store(height, Ordering::Relaxed);
            for tab in &self.tabs {
                tab.resize(width, height);
            }
            self.request_active_snapshot();
        }

        fn dispatch_key_event(&self, event: buffr_engine::NeutralKeyEvent) {
            if let Some(idx) = self.active_idx {
                if let Some(tab) = self.tabs.get(idx) {
                    super::super::input::macos::dispatch_key_event(&tab.web_view, &event);
                    self.request_active_snapshot();
                }
            }
        }

        fn dispatch_mouse_move(&self, x: i32, y: i32, modifiers: u32) {
            if let Some(idx) = self.active_idx {
                if let Some(tab) = self.tabs.get(idx) {
                    super::super::input::macos::dispatch_mouse_move(&tab.web_view, x, y, modifiers);
                    self.request_active_snapshot();
                }
            }
        }

        fn dispatch_mouse_click(
            &self,
            x: i32,
            y: i32,
            button: buffr_engine::MouseButton,
            mouse_up: bool,
            click_count: i32,
            modifiers: u32,
        ) {
            if let Some(idx) = self.active_idx {
                if let Some(tab) = self.tabs.get(idx) {
                    super::super::input::macos::dispatch_mouse_click(
                        &tab.web_view,
                        x,
                        y,
                        &button,
                        mouse_up,
                        click_count,
                        modifiers,
                    );
                    self.request_active_snapshot();
                }
            }
        }

        fn dispatch_mouse_wheel(&self, x: i32, y: i32, delta_x: i32, delta_y: i32, modifiers: u32) {
            if let Some(idx) = self.active_idx {
                if let Some(tab) = self.tabs.get(idx) {
                    super::super::input::macos::dispatch_mouse_wheel(
                        &tab.web_view,
                        x,
                        y,
                        delta_x,
                        delta_y,
                        modifiers,
                    );
                    self.request_active_snapshot();
                }
            }
        }

        fn close_all(&mut self) {
            self.tabs.clear();
            self.active_idx = None;
            self.snapshot_to_engine_state();
            super::super::osr::paint_blank(&self.frame, &self.view);
        }

        fn can_go_back(&self) -> bool {
            self.active_idx
                .and_then(|i| self.tabs.get(i))
                .is_some_and(|t| t.can_go_back())
        }

        fn can_go_forward(&self) -> bool {
            self.active_idx
                .and_then(|i| self.tabs.get(i))
                .is_some_and(|t| t.can_go_forward())
        }

        /// Evaluate `code` in the active tab's JS context (fire-and-forget).
        fn eval_js(&self, code: &str) {
            if let Some(idx) = self.active_idx {
                if let Some(tab) = self.tabs.get(idx) {
                    tab.eval_js(code);
                }
            }
        }

        /// Apply a zoom level to the active tab's WKWebView and snapshot the
        /// updated TabState into EngineState so `active_zoom_level()` can read
        /// it from any thread without a main-queue round-trip.
        fn set_zoom(&mut self, level: f64) {
            if let Some(idx) = self.active_idx {
                if let Some(tab) = self.tabs.get_mut(idx) {
                    tab.set_zoom(level);
                    self.snapshot_to_engine_state();
                }
            }
        }
    }

    // ── Commands ──────────────────────────────────────────────────────────────

    pub(crate) enum Command {
        OpenTab {
            url: String,
            reply: mpsc::SyncSender<Result<TabId, WebKitCocoaError>>,
        },
        CloseTab {
            id: TabId,
            reply: mpsc::SyncSender<Result<bool, WebKitCocoaError>>,
        },
        CloseAll,
        SelectTab {
            id: TabId,
        },
        CycleTab {
            forward: bool,
        },
        Navigate {
            url: String,
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
        KeyEvent {
            event: buffr_engine::NeutralKeyEvent,
        },
        MouseMove {
            x: i32,
            y: i32,
            modifiers: u32,
        },
        MouseClick {
            x: i32,
            y: i32,
            button: buffr_engine::MouseButton,
            mouse_up: bool,
            click_count: i32,
            modifiers: u32,
        },
        MouseWheel {
            x: i32,
            y: i32,
            delta_x: i32,
            delta_y: i32,
            modifiers: u32,
        },
        QueryCanGoBack {
            reply: mpsc::SyncSender<bool>,
        },
        QueryCanGoForward {
            reply: mpsc::SyncSender<bool>,
        },
        /// Set the active tab's page zoom level. Clamped to [0.25, 5.0].
        ///
        /// Dispatched to the GCD main queue and calls
        /// `WKWebView::setPageZoom` (macOS 11+).
        /// Source: objc2-web-kit-0.3.2/src/generated/WKWebView.rs:809-811.
        SetZoom(f64),
        /// Evaluate `code` in the active tab's JS context (fire-and-forget).
        ///
        /// Dispatched to the GCD main queue and calls
        /// `WKWebView::evaluateJavaScript:completionHandler:`. The completion
        /// handler is ignored — this matches the trait's fire-and-forget contract.
        EvalJs {
            code: String,
        },
        Shutdown,
    }

    // ── WorkerHandle ──────────────────────────────────────────────────────────

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
        ) -> Result<T, WebKitCocoaError> {
            let (reply_tx, reply_rx) = mpsc::sync_channel(1);
            self.send(build(reply_tx));
            reply_rx
                .recv_timeout(Duration::from_secs(30))
                .map_err(|_| WebKitCocoaError::Timeout)
        }
    }

    // ── dispatch helpers ──────────────────────────────────────────────────────

    /// Dispatch a closure to the macOS main queue asynchronously.
    ///
    /// `DispatchQueue::main()` returns `&'static DispatchQueue`.
    /// Confirmed: dispatch2-0.3.1/src/queue.rs:108-114.
    ///
    /// `DispatchQueue::exec_async<F: FnOnce() + Send + 'static>` confirmed:
    /// dispatch2-0.3.1/src/queue.rs:134-143.
    ///
    /// # Safety
    ///
    /// The closure must be `Send + 'static`. All captures must be `Send`
    /// (i.e. `Arc<Mutex<…>>` rather than `Rc<RefCell<…>>`).
    fn dispatch_main_async<F: FnOnce() + Send + 'static>(f: F) {
        // DispatchQueue::main() confirmed: dispatch2/src/queue.rs:108.
        // exec_async confirmed: dispatch2/src/queue.rs:134.
        DispatchQueue::main().exec_async(f);
    }

    // ── Worker thread ─────────────────────────────────────────────────────────

    /// Spawn the background worker thread.
    ///
    /// The worker receives `Command`s from the engine and either:
    /// - Handles synchronous queries directly from `EngineState` (under Mutex), or
    /// - Dispatches closures to the GCD main queue for WKWebView mutations.
    pub(crate) fn spawn(
        initial_url: &str,
        _width: u32,
        _height: u32,
        frame: SharedOsrFrame,
        view: SharedOsrViewState,
        engine_state: Arc<Mutex<EngineState>>,
    ) -> Result<WorkerHandle, WebKitCocoaError> {
        let (tx, rx) = mpsc::sync_channel::<Command>(64);
        let initial_url = initial_url.to_owned();

        // SAFETY: `MainQueueBox<RuntimeState>` is `Send` by our invariant —
        // all closures that touch `rt` are dispatched via `dispatch_main_async`
        // and run exclusively on the serial GCD main queue.
        let rt: Arc<Mutex<MainQueueBox<RuntimeState>>> = Arc::new(Mutex::new(MainQueueBox(
            RuntimeState::new(frame, view, Arc::clone(&engine_state)),
        )));

        // Submit the initial open_tab to the main queue.
        {
            let rt_init = Arc::clone(&rt);
            let url_init = initial_url.clone();
            dispatch_main_async(move || {
                if let Ok(mut guard) = rt_init.lock() {
                    if let Err(e) = guard.0.open_tab(&url_init) {
                        tracing::error!("webkit-cocoa worker: initial open_tab failed: {e}");
                    }
                }
            });
        }

        // ── Audio-activity poll (500 ms recurring, main-queue) ────────────────
        //
        // WKWebView::isPlayingAudio must be called on the main thread.  We use a
        // self-rescheduling `dispatch_main_async` + `after` pattern: each tick
        // reads all tabs, ORs `isPlayingAudio`, stores the result in the atomic,
        // then schedules the next tick.
        {
            let rt_audio = Arc::clone(&rt);
            let audio_flag = Arc::clone(
                &engine_state
                    .lock()
                    .map(|g| Arc::clone(&g.audio_active))
                    .unwrap_or_else(|_| Arc::new(AtomicBool::new(false))),
            );

            // Recursive tick function, expressed as a plain function rather than
            // a Fn closure to keep the capture set simple.
            fn schedule_audio_tick(
                rt: Arc<Mutex<MainQueueBox<RuntimeState>>>,
                audio_flag: Arc<AtomicBool>,
            ) {
                let when =
                    DispatchTime::try_from(Duration::from_millis(500)).unwrap_or(DispatchTime::NOW);
                let _ = DispatchQueue::main().after(when, move || {
                    // Poll all WKWebView instances on the main queue.
                    let any_audio = rt.lock().is_ok_and(|guard| {
                        use objc2::msg_send;
                        guard.0.tabs.iter().any(|tab| {
                            // SAFETY: `isPlayingAudio` is a read-only selector on
                            // WKWebView that only inspects internal media engine
                            // state.  It is safe to call from the GCD main queue
                            // as long as the WKWebView is alive, which is
                            // guaranteed here because `TabEntry` holds a strong
                            // `Retained<WKWebView>` reference.
                            unsafe { msg_send![&*tab.web_view, isPlayingAudio] }
                        })
                    });
                    audio_flag.store(any_audio, Ordering::Relaxed);
                    // Re-schedule the next tick.
                    schedule_audio_tick(rt, audio_flag);
                });
            }

            schedule_audio_tick(rt_audio, audio_flag);
        }

        // Spawn the worker thread.
        thread::Builder::new()
            .name("buffr-webkit-cocoa-worker".into())
            .spawn(move || {
                tracing::info!("webkit-cocoa worker: started");
                loop {
                    match rx.recv() {
                        Ok(cmd) => {
                            if handle_command(cmd, &rt, &engine_state) {
                                break;
                            }
                        }
                        Err(_) => {
                            tracing::info!("webkit-cocoa worker: channel closed");
                            break;
                        }
                    }
                }
                tracing::info!("webkit-cocoa worker: exited");
            })
            .map_err(|e| WebKitCocoaError::InitFailed(e.to_string()))?;

        Ok(WorkerHandle { tx })
    }

    fn handle_command(
        cmd: Command,
        rt: &Arc<Mutex<MainQueueBox<RuntimeState>>>,
        _engine_state: &Arc<Mutex<EngineState>>,
    ) -> bool {
        match cmd {
            // ── Synchronous queries ───────────────────────────────────────────
            //
            // QueryCanGoBack / QueryCanGoForward: these read from RuntimeState
            // which holds the live WKWebView. They must be dispatched to the
            // main queue and reply via the channel.
            Command::QueryCanGoBack { reply } => {
                let rt2 = Arc::clone(rt);
                dispatch_main_async(move || {
                    let v = rt2.lock().is_ok_and(|r| r.0.can_go_back());
                    let _ = reply.send(v);
                });
            }
            Command::QueryCanGoForward { reply } => {
                let rt2 = Arc::clone(rt);
                dispatch_main_async(move || {
                    let v = rt2.lock().is_ok_and(|r| r.0.can_go_forward());
                    let _ = reply.send(v);
                });
            }
            Command::Shutdown => {
                return true;
            }

            // ── Mutations dispatched to main queue ────────────────────────────
            Command::OpenTab { url, reply } => {
                let rt2 = Arc::clone(rt);
                dispatch_main_async(move || {
                    let result = rt2
                        .lock()
                        .map_err(|_| WebKitCocoaError::WorkerGone)
                        .and_then(|mut r| r.0.open_tab(&url));
                    let _ = reply.send(result);
                });
            }
            Command::CloseTab { id, reply } => {
                let rt2 = Arc::clone(rt);
                dispatch_main_async(move || {
                    let result = rt2
                        .lock()
                        .map_err(|_| WebKitCocoaError::WorkerGone)
                        .and_then(|mut r| r.0.close_tab(id));
                    let _ = reply.send(result);
                });
            }
            Command::CloseAll => {
                let rt2 = Arc::clone(rt);
                dispatch_main_async(move || {
                    if let Ok(mut r) = rt2.lock() {
                        r.0.close_all();
                    }
                });
            }
            Command::SelectTab { id } => {
                let rt2 = Arc::clone(rt);
                dispatch_main_async(move || {
                    if let Ok(mut r) = rt2.lock() {
                        r.0.select_tab(id);
                    }
                });
            }
            Command::CycleTab { forward } => {
                let rt2 = Arc::clone(rt);
                dispatch_main_async(move || {
                    if let Ok(mut r) = rt2.lock() {
                        r.0.cycle_tab(forward);
                    }
                });
            }
            Command::Navigate { url } => {
                let rt2 = Arc::clone(rt);
                dispatch_main_async(move || {
                    if let Ok(mut r) = rt2.lock() {
                        r.0.navigate(&url);
                    }
                });
            }
            Command::GoBack => {
                let rt2 = Arc::clone(rt);
                dispatch_main_async(move || {
                    if let Ok(mut r) = rt2.lock() {
                        r.0.go_back();
                    }
                });
            }
            Command::GoForward => {
                let rt2 = Arc::clone(rt);
                dispatch_main_async(move || {
                    if let Ok(mut r) = rt2.lock() {
                        r.0.go_forward();
                    }
                });
            }
            Command::Reload => {
                let rt2 = Arc::clone(rt);
                dispatch_main_async(move || {
                    if let Ok(mut r) = rt2.lock() {
                        r.0.reload();
                    }
                });
            }
            Command::Stop => {
                let rt2 = Arc::clone(rt);
                dispatch_main_async(move || {
                    if let Ok(mut r) = rt2.lock() {
                        r.0.stop();
                    }
                });
            }
            Command::Resize { width, height } => {
                let rt2 = Arc::clone(rt);
                dispatch_main_async(move || {
                    if let Ok(mut r) = rt2.lock() {
                        r.0.resize(width, height);
                    }
                });
            }
            Command::ForcePaint => {
                let rt2 = Arc::clone(rt);
                dispatch_main_async(move || {
                    if let Ok(r) = rt2.lock() {
                        r.0.request_active_snapshot();
                    }
                });
            }
            Command::KeyEvent { event } => {
                let rt2 = Arc::clone(rt);
                dispatch_main_async(move || {
                    if let Ok(r) = rt2.lock() {
                        r.0.dispatch_key_event(event);
                    }
                });
            }
            Command::MouseMove { x, y, modifiers } => {
                let rt2 = Arc::clone(rt);
                dispatch_main_async(move || {
                    if let Ok(r) = rt2.lock() {
                        r.0.dispatch_mouse_move(x, y, modifiers);
                    }
                });
            }
            Command::MouseClick {
                x,
                y,
                button,
                mouse_up,
                click_count,
                modifiers,
            } => {
                let rt2 = Arc::clone(rt);
                dispatch_main_async(move || {
                    if let Ok(r) = rt2.lock() {
                        r.0.dispatch_mouse_click(x, y, button, mouse_up, click_count, modifiers);
                    }
                });
            }
            Command::MouseWheel {
                x,
                y,
                delta_x,
                delta_y,
                modifiers,
            } => {
                let rt2 = Arc::clone(rt);
                dispatch_main_async(move || {
                    if let Ok(r) = rt2.lock() {
                        r.0.dispatch_mouse_wheel(x, y, delta_x, delta_y, modifiers);
                    }
                });
            }
            Command::SetZoom(level) => {
                let rt2 = Arc::clone(rt);
                dispatch_main_async(move || {
                    if let Ok(mut r) = rt2.lock() {
                        r.0.set_zoom(level);
                    }
                });
            }
            Command::EvalJs { code } => {
                let rt2 = Arc::clone(rt);
                dispatch_main_async(move || {
                    if let Ok(r) = rt2.lock() {
                        r.0.eval_js(&code);
                    }
                });
            }
        }
        false
    }
}
