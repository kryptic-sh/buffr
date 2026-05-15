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
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, mpsc};
    use std::thread;
    use std::time::Duration;

    use buffr_engine::{SharedOsrFrame, SharedOsrViewState, TabId, TabSummary};

    use dispatch2::DispatchQueue;

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
    }

    impl EngineState {
        pub(crate) fn new() -> Self {
            EngineState {
                tabs: Vec::new(),
                active_idx: None,
            }
        }

        pub(crate) fn active_id(&self) -> Option<TabId> {
            self.active_idx.and_then(|i| self.tabs.get(i)).map(|t| t.id)
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

    // ── RuntimeState (main-thread only) ───────────────────────────────────────

    /// Mutable per-tab WKWebView table. Accessed only from the GCD main queue.
    ///
    /// Wrapped in `Arc<Mutex<…>>` (not `Rc<RefCell<…>>`) because
    /// `dispatch2::DispatchQueue::exec_async` requires `Send` closures.
    /// The GCD main queue is serial so the `Mutex` is never contended.
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
                    })
                    .collect();
                st.active_idx = self.active_idx;
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
        width: u32,
        height: u32,
        frame: SharedOsrFrame,
        view: SharedOsrViewState,
        engine_state: Arc<Mutex<EngineState>>,
    ) -> Result<WorkerHandle, WebKitCocoaError> {
        let (tx, rx) = mpsc::sync_channel::<Command>(64);
        let initial_url = initial_url.to_owned();

        let rt = Arc::new(Mutex::new(RuntimeState::new(
            frame,
            view,
            Arc::clone(&engine_state),
        )));

        // Submit the initial open_tab to the main queue.
        {
            let rt_init = Arc::clone(&rt);
            let url_init = initial_url.clone();
            dispatch_main_async(move || {
                if let Ok(mut rt) = rt_init.lock() {
                    if let Err(e) = rt.open_tab(&url_init) {
                        tracing::error!("webkit-cocoa worker: initial open_tab failed: {e}");
                    }
                }
            });
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
        rt: &Arc<Mutex<RuntimeState>>,
        engine_state: &Arc<Mutex<EngineState>>,
    ) -> bool {
        match cmd {
            // ── Synchronous queries ───────────────────────────────────────────
            Command::QueryCanGoBack { reply } => {
                let v = rt.lock().is_ok_and(|r| r.can_go_back());
                let _ = reply.send(v);
            }
            Command::QueryCanGoForward { reply } => {
                let v = rt.lock().is_ok_and(|r| r.can_go_forward());
                let _ = reply.send(v);
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
                        .and_then(|mut r| r.open_tab(&url));
                    let _ = reply.send(result);
                });
            }
            Command::CloseTab { id, reply } => {
                let rt2 = Arc::clone(rt);
                dispatch_main_async(move || {
                    let result = rt2
                        .lock()
                        .map_err(|_| WebKitCocoaError::WorkerGone)
                        .and_then(|mut r| r.close_tab(id));
                    let _ = reply.send(result);
                });
            }
            Command::CloseAll => {
                let rt2 = Arc::clone(rt);
                dispatch_main_async(move || {
                    if let Ok(mut r) = rt2.lock() {
                        r.close_all();
                    }
                });
            }
            Command::SelectTab { id } => {
                let rt2 = Arc::clone(rt);
                dispatch_main_async(move || {
                    if let Ok(mut r) = rt2.lock() {
                        r.select_tab(id);
                    }
                });
            }
            Command::CycleTab { forward } => {
                let rt2 = Arc::clone(rt);
                dispatch_main_async(move || {
                    if let Ok(mut r) = rt2.lock() {
                        r.cycle_tab(forward);
                    }
                });
            }
            Command::Navigate { url } => {
                let rt2 = Arc::clone(rt);
                dispatch_main_async(move || {
                    if let Ok(mut r) = rt2.lock() {
                        r.navigate(&url);
                    }
                });
            }
            Command::GoBack => {
                let rt2 = Arc::clone(rt);
                dispatch_main_async(move || {
                    if let Ok(mut r) = rt2.lock() {
                        r.go_back();
                    }
                });
            }
            Command::GoForward => {
                let rt2 = Arc::clone(rt);
                dispatch_main_async(move || {
                    if let Ok(mut r) = rt2.lock() {
                        r.go_forward();
                    }
                });
            }
            Command::Reload => {
                let rt2 = Arc::clone(rt);
                dispatch_main_async(move || {
                    if let Ok(mut r) = rt2.lock() {
                        r.reload();
                    }
                });
            }
            Command::Stop => {
                let rt2 = Arc::clone(rt);
                dispatch_main_async(move || {
                    if let Ok(mut r) = rt2.lock() {
                        r.stop();
                    }
                });
            }
            Command::Resize { width, height } => {
                let rt2 = Arc::clone(rt);
                dispatch_main_async(move || {
                    if let Ok(mut r) = rt2.lock() {
                        r.resize(width, height);
                    }
                });
            }
            Command::ForcePaint => {
                let rt2 = Arc::clone(rt);
                dispatch_main_async(move || {
                    if let Ok(r) = rt2.lock() {
                        r.request_active_snapshot();
                    }
                });
            }
            Command::KeyEvent { event } => {
                let rt2 = Arc::clone(rt);
                dispatch_main_async(move || {
                    if let Ok(r) = rt2.lock() {
                        r.dispatch_key_event(event);
                    }
                });
            }
            Command::MouseMove { x, y, modifiers } => {
                let rt2 = Arc::clone(rt);
                dispatch_main_async(move || {
                    if let Ok(r) = rt2.lock() {
                        r.dispatch_mouse_move(x, y, modifiers);
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
                        r.dispatch_mouse_click(x, y, button, mouse_up, click_count, modifiers);
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
                        r.dispatch_mouse_wheel(x, y, delta_x, delta_y, modifiers);
                    }
                });
            }
        }
        false
    }
}
