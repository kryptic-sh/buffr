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

    use buffr_core::find::FindResultSink;
    use buffr_core::hint::HintEventSink;
    use buffr_engine::popup::PopupQueue;
    use buffr_engine::{SharedOsrFrame, SharedOsrViewState, TabId, TabSummary};

    use dispatch2::{DispatchQueue, DispatchTime};

    use super::super::error::WebKitCocoaError;
    use super::super::runtime::macos::{TabEntry, TabState};

    // ── Closed-tab undo stack entry ───────────────────────────────────────────

    /// One entry on the recently-closed tab undo stack.
    ///
    /// URL-only restoration: WKWebView is released on close; reopen triggers a
    /// fresh load at the original strip position. History / scroll state are
    /// not preserved (acceptable trade-off for the macOS WebKit backend).
    #[derive(Debug, Clone)]
    pub(crate) struct ClosedEntry {
        pub url: String,
        pub original_idx: usize,
    }

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
        /// Pending favicon updates queued by the navigation delegate on the GCD
        /// main queue.  Drained by `BrowserEngine::drain_favicon_updates` from
        /// any thread.
        ///
        /// Each navigation completion pushes one entry using the heuristic URL
        /// `<page_url>/favicon.ico`.  Pixel data is not available without an
        /// async fetch; width/height/pixels are left at zero/empty as a
        /// navigation-completed sentinel.  A future phase can perform the fetch
        /// off-thread and fill real bitmap data.
        pub favicon_updates: Arc<Mutex<Vec<buffr_engine::FaviconUpdate>>>,
        /// OSR sleep state flag.
        ///
        /// When `true`, the audio-activity poll and OSR tick skip their work to
        /// reduce CPU usage while the tab is backgrounded.  Set by
        /// `BrowserEngine::osr_sleep`; read by the recurring tick functions.
        pub osr_sleep: Arc<AtomicBool>,
        /// Cached "any video/audio active?" flag.
        ///
        /// Written by the WKScriptMessageHandler when the JS media probe emits
        /// `__buffr_media__:true/false`; read lock-free by `any_video_active`.
        pub video_active: Arc<AtomicBool>,
        /// One-slot mailbox for cursor changes.
        ///
        /// Written by the WKScriptMessageHandler when the JS cursor listener
        /// emits `__buffr_cursor__:<css>`. Drained by `take_cursor_change`.
        pub cursor_change: Arc<Mutex<Option<(i32, u32)>>>,
        /// Shared browsing-history store. When `Some`, the navigation delegate
        /// calls [`buffr_history::History::record_visit`] on each
        /// `didFinishNavigation` callback.
        pub history: Option<Arc<buffr_history::History>>,
        /// Shared downloads store. When `Some`, the WKDownload delegate records
        /// downloads started, completed, or failed.
        pub downloads: Option<Arc<buffr_downloads::Downloads>>,
        /// Find-result sink. When `Some`, the `find_string` completion handler
        /// pushes a [`buffr_core::find::FindResult`] after each search.
        pub find_sink: Option<FindResultSink>,
        /// Popup URL queue. URLs intercepted by WKUIDelegate `createWebView` are
        /// pushed here so the apps layer can open them as new tabs.
        pub popup_queue: PopupQueue,
        /// Stack of recently closed tabs (most-recent last).
        ///
        /// Pushed by `RuntimeState::close_tab` with the URL and original strip
        /// position; popped by `reopen_closed_tab` which opens a fresh tab at
        /// the original index (clamped to the current strip length).
        pub closed_stack: Vec<ClosedEntry>,
    }

    impl EngineState {
        pub(crate) fn new() -> Self {
            EngineState {
                tabs: Vec::new(),
                active_idx: None,
                audio_active: Arc::new(AtomicBool::new(false)),
                loading_active: Arc::new(AtomicBool::new(false)),
                favicon_updates: Arc::new(Mutex::new(Vec::new())),
                osr_sleep: Arc::new(AtomicBool::new(false)),
                video_active: Arc::new(AtomicBool::new(false)),
                cursor_change: Arc::new(Mutex::new(None)),
                history: None,
                downloads: None,
                find_sink: None,
                popup_queue: buffr_engine::popup::new_popup_queue(),
                closed_stack: Vec::new(),
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
        hint_sink: HintEventSink,
        /// Cloned from `EngineState::popup_queue`; passed into each new tab's
        /// `BuffrUiDelegate` so the createWebView callback can push URLs directly
        /// without locking the full EngineState.
        popup_queue: PopupQueue,
    }

    impl RuntimeState {
        fn new(
            frame: SharedOsrFrame,
            view: SharedOsrViewState,
            engine_state: Arc<Mutex<EngineState>>,
            hint_sink: HintEventSink,
            popup_queue: PopupQueue,
        ) -> Self {
            RuntimeState {
                tabs: Vec::new(),
                active_idx: None,
                next_id: 1,
                frame,
                view,
                engine_state,
                hint_sink,
                popup_queue,
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
            let entry = TabEntry::open(
                id,
                url,
                w,
                h,
                Arc::clone(&self.engine_state),
                Arc::clone(&self.hint_sink),
                Arc::clone(&self.popup_queue),
            )?;
            self.tabs.push(entry);
            self.active_idx = Some(self.tabs.len() - 1);
            self.snapshot_to_engine_state();
            self.request_active_snapshot();
            Ok(id)
        }

        /// Open a new tab and insert it at `insert_idx` in the strip.
        ///
        /// Out-of-bounds indices clamp to the end. The new tab becomes active.
        fn open_tab_at(&mut self, url: &str, insert_idx: usize) -> Result<TabId, WebKitCocoaError> {
            let id = self.open_tab(url)?;
            // `open_tab` appended and set active to len-1.
            let appended_idx = self.tabs.len() - 1;
            let clamped = insert_idx.min(appended_idx);
            if clamped != appended_idx {
                let tab = self.tabs.remove(appended_idx);
                self.tabs.insert(clamped, tab);
                // Fix active_idx: removal from appended_idx then insert at clamped
                // shifts indices in [clamped, appended_idx) up by one.
                if let Some(a) = self.active_idx {
                    if a >= clamped && a < appended_idx {
                        self.active_idx = Some(a + 1);
                    }
                }
                // New tab is at clamped; make it active.
                self.active_idx = Some(clamped);
                self.snapshot_to_engine_state();
                self.request_active_snapshot();
            }
            Ok(id)
        }

        /// Move the tab at `from` to position `to`. Clamp to valid range.
        /// Same-position is a no-op. Fixes `active_idx` after the move.
        fn move_tab(&mut self, from: usize, to: usize) {
            let len = self.tabs.len();
            if len == 0 || from >= len || from == to {
                return;
            }
            let to = to.min(len - 1);
            if to == from {
                return;
            }
            let tab = self.tabs.remove(from);
            self.tabs.insert(to, tab);
            // Fix active_idx so it still points at the same tab.
            if let Some(a) = self.active_idx {
                let new_a = if a == from {
                    to
                } else if from < a && to >= a {
                    a - 1
                } else if from > a && to <= a {
                    a + 1
                } else {
                    a
                };
                self.active_idx = Some(new_a);
            }
            self.snapshot_to_engine_state();
        }

        fn close_tab(&mut self, id: TabId) -> Result<bool, WebKitCocoaError> {
            let idx = self
                .tab_index_by_id(id)
                .ok_or(WebKitCocoaError::TabNotFound(id))?;
            // Push to closed stack before releasing the WKWebView.
            // URL-only restoration — WKWebView is not kept alive.
            let closed_url = self.tabs[idx].url.clone();
            if let Ok(mut st) = self.engine_state.lock() {
                st.closed_stack.push(ClosedEntry {
                    url: closed_url,
                    original_idx: idx,
                });
            }
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

        /// Start or stop an in-page find session on the active tab.
        ///
        /// Uses `WKWebView::findString:withConfiguration:completionHandler:` (macOS
        /// 14 / WebKit 617+).  `query=""` cancels the active highlight.
        ///
        /// When `self.engine_state` carries a `find_sink`, the completion handler
        /// pushes a [`buffr_core::find::FindResult`] derived from
        /// `WKFindResult::matchFound`.  The count is capped at 1 because
        /// `WKFindResult` exposes only a boolean; a future phase can use
        /// `WKFindResult::matchesCount` if Apple makes that API public.
        ///
        /// If `WKFindConfiguration` is unavailable (macOS < 14) we fall through
        /// to a no-op and log at `debug!`.
        fn find_string(&self, query: &str, forward: bool) {
            use core::ptr::NonNull;

            use block2::RcBlock;
            use objc2::MainThreadMarker;
            use objc2_foundation::NSString;
            use objc2_web_kit::{WKFindConfiguration, WKFindResult};

            if let Some(idx) = self.active_idx {
                if let Some(tab) = self.tabs.get(idx) {
                    // SAFETY: find_string is called only from closures dispatched
                    // via dispatch_main_async, which run on the GCD main queue.
                    let mtm = unsafe { MainThreadMarker::new_unchecked() };

                    let ns_query = NSString::from_str(query);

                    // Build WKFindConfiguration using the typed binding
                    // (objc2-web-kit feature "WKFindConfiguration").
                    // `WKFindConfiguration::new(mtm)` requires `MainThreadOnly`.
                    let cfg = unsafe { WKFindConfiguration::new(mtm) };

                    // Set backwards = !forward (default is forward).
                    unsafe { cfg.setBackwards(!forward) };

                    // Clone the engine_state to read find_sink inside the block.
                    // The block is `Send + 'static`, so captures must be too.
                    // `Arc<Mutex<EngineState>>` satisfies that contract.
                    let state_for_block = Arc::clone(&self.engine_state);

                    // Build the completion handler block.
                    // Signature: `void (^)(WKFindResult *)`.
                    // The block receives a `NonNull<WKFindResult>` (objc2 convention).
                    let completion = RcBlock::new(move |result: NonNull<WKFindResult>| {
                        // SAFETY: `result` is a valid `WKFindResult *` provided by
                        // WebKit on the GCD main queue; the pointer is alive for
                        // the duration of this callback.
                        let match_found = unsafe { result.as_ref().matchFound() };
                        tracing::debug!(
                            match_found,
                            "webkit-cocoa: find completion (WKFindResult)"
                        );
                        if let Ok(st) = state_for_block.lock() {
                            if let Some(sink) = &st.find_sink {
                                if let Ok(mut guard) = sink.lock() {
                                    *guard = Some(buffr_core::find::FindResult {
                                        count: if match_found { 1 } else { 0 },
                                        current: if match_found { 1 } else { 0 },
                                        final_update: true,
                                    });
                                }
                            }
                        }
                    });

                    // findString:withConfiguration:completionHandler:
                    // Binding: WKWebView.rs (feature WKFindConfiguration + WKFindResult).
                    // `&*completion` coerces RcBlock<F> to &DynBlock<dyn Fn(…)>.
                    // SAFETY: all arguments are valid ObjC objects on the main thread.
                    unsafe {
                        tab.web_view.findString_withConfiguration_completionHandler(
                            &ns_query,
                            Some(&cfg),
                            &*completion,
                        );
                    }
                }
            }
        }

        /// Read the system clipboard text.  Must be called on the main thread.
        ///
        /// Uses `NSPasteboard::generalPasteboard` and reads the first plain-text
        /// item.  Returns `None` when the clipboard is empty or contains non-text.
        ///
        /// NSPasteboard confirmed: objc2-app-kit NSPasteboard (AppKit).
        /// We reach it via raw `AnyClass::get` + `msg_send!` to avoid depending
        /// on the `NSPasteboard` binding feature in `objc2-app-kit`.
        fn clipboard_read() -> Option<String> {
            use std::ffi::CStr;

            use objc2::msg_send;
            use objc2_foundation::NSString;

            unsafe {
                // AnyClass::get takes &CStr (objc2 0.6).
                let pb_class_name: &CStr = c"NSPasteboard";
                let cls: *mut objc2::runtime::AnyClass =
                    objc2::runtime::AnyClass::get(pb_class_name)
                        .map(|c| c as *const _ as *mut _)
                        .unwrap_or(std::ptr::null_mut());
                if cls.is_null() {
                    return None;
                }
                // +[NSPasteboard generalPasteboard]
                let pb: *mut objc2::runtime::AnyObject = msg_send![cls, generalPasteboard];
                if pb.is_null() {
                    return None;
                }
                // -[NSPasteboard stringForType:] with NSPasteboardTypeString.
                // NSPasteboardTypeString UTI = "public.utf8-plain-text"
                let type_str = NSString::from_str("public.utf8-plain-text");
                let result: *mut NSString = msg_send![pb, stringForType: &*type_str];
                if result.is_null() {
                    None
                } else {
                    Some((*result).to_string())
                }
            }
        }

        /// Write `text` to the system clipboard.  Must be called on the main thread.
        ///
        /// Uses `NSPasteboard::generalPasteboard`, clears it, then writes a plain
        /// UTF-8 string.
        fn clipboard_write(text: &str) {
            use std::ffi::CStr;

            use objc2::msg_send;
            use objc2_foundation::NSString;

            unsafe {
                let pb_class_name: &CStr = c"NSPasteboard";
                let cls: *mut objc2::runtime::AnyClass =
                    objc2::runtime::AnyClass::get(pb_class_name)
                        .map(|c| c as *const _ as *mut _)
                        .unwrap_or(std::ptr::null_mut());
                if cls.is_null() {
                    return;
                }
                // +[NSPasteboard generalPasteboard]
                let pb: *mut objc2::runtime::AnyObject = msg_send![cls, generalPasteboard];
                if pb.is_null() {
                    return;
                }
                // -[NSPasteboard clearContents] → NSInteger (number of items cleared)
                let _: objc2::ffi::NSInteger = msg_send![pb, clearContents];
                // -[NSPasteboard setString:forType:] → BOOL
                let ns_text = NSString::from_str(text);
                let type_str = NSString::from_str("public.utf8-plain-text");
                let _: objc2::runtime::Bool =
                    msg_send![pb, setString: &*ns_text, forType: &*type_str];
            }
        }
    }

    // ── Commands ──────────────────────────────────────────────────────────────

    pub(crate) enum Command {
        OpenTab {
            url: String,
            reply: mpsc::SyncSender<Result<TabId, WebKitCocoaError>>,
        },
        /// Open a new tab at `insert_idx` in the strip. Clamps OOB.
        OpenTabAt {
            url: String,
            insert_idx: usize,
            reply: mpsc::SyncSender<Result<TabId, WebKitCocoaError>>,
        },
        CloseTab {
            id: TabId,
            reply: mpsc::SyncSender<Result<bool, WebKitCocoaError>>,
        },
        CloseAll,
        /// Move the tab at `from` to position `to`. Clamps OOB. No-op on same-pos.
        MoveTab {
            from: usize,
            to: usize,
        },
        /// Reopen the most-recently-closed tab. Reply is the new `TabId`, or
        /// `None` when the closed stack is empty.
        ReopenClosed {
            reply: mpsc::SyncSender<Result<Option<TabId>, WebKitCocoaError>>,
        },
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
        /// Begin an in-page find session using WKFindConfiguration (macOS 16+ /
        /// WebKit 619+).  Falls back to `document.execCommand` on older systems.
        ///
        /// `query=""` acts as a stop-find (clears the active highlight).
        StartFind {
            query: String,
            forward: bool,
        },
        /// Stop the active find session (alias for `StartFind { query: "" }`).
        StopFind,
        /// Read the system clipboard text on the main thread and reply.
        ///
        /// NSPasteboard must be called on the main thread.  The reply sender
        /// carries `Option<String>` — `None` if the clipboard is empty/non-text.
        ClipboardRead {
            reply: mpsc::SyncSender<Option<String>>,
        },
        /// Write `text` to the system clipboard on the main thread.
        ///
        /// NSPasteboard must be called on the main thread.
        ClipboardWrite {
            text: String,
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
        hint_sink: HintEventSink,
    ) -> Result<WorkerHandle, WebKitCocoaError> {
        let (tx, rx) = mpsc::sync_channel::<Command>(64);
        let initial_url = initial_url.to_owned();

        // Extract the popup_queue from engine_state so RuntimeState can clone it
        // into each new tab's BuffrUiDelegate without locking the full engine_state.
        let popup_queue_for_rt = engine_state
            .lock()
            .map(|g| Arc::clone(&g.popup_queue))
            .unwrap_or_else(|_| buffr_engine::popup::new_popup_queue());

        // SAFETY: `MainQueueBox<RuntimeState>` is `Send` by our invariant —
        // all closures that touch `rt` are dispatched via `dispatch_main_async`
        // and run exclusively on the serial GCD main queue.
        let rt: Arc<Mutex<MainQueueBox<RuntimeState>>> =
            Arc::new(Mutex::new(MainQueueBox(RuntimeState::new(
                frame,
                view,
                Arc::clone(&engine_state),
                hint_sink,
                popup_queue_for_rt,
            ))));

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
        //
        // The tick respects `osr_sleep`: when true, audio polling is skipped and
        // the result is frozen at false to avoid unnecessary wakeups.
        {
            let rt_audio = Arc::clone(&rt);
            let audio_flag = Arc::clone(
                &engine_state
                    .lock()
                    .map(|g| Arc::clone(&g.audio_active))
                    .unwrap_or_else(|_| Arc::new(AtomicBool::new(false))),
            );
            let osr_sleep_audio = Arc::clone(
                &engine_state
                    .lock()
                    .map(|g| Arc::clone(&g.osr_sleep))
                    .unwrap_or_else(|_| Arc::new(AtomicBool::new(false))),
            );

            // Recursive tick function, expressed as a plain function rather than
            // a Fn closure to keep the capture set simple.
            fn schedule_audio_tick(
                rt: Arc<Mutex<MainQueueBox<RuntimeState>>>,
                audio_flag: Arc<AtomicBool>,
                osr_sleep: Arc<AtomicBool>,
            ) {
                let when =
                    DispatchTime::try_from(Duration::from_millis(500)).unwrap_or(DispatchTime::NOW);
                let _ = DispatchQueue::main().after(when, move || {
                    // When osr_sleep is active, skip polling and keep audio false.
                    if osr_sleep.load(Ordering::Relaxed) {
                        audio_flag.store(false, Ordering::Relaxed);
                        schedule_audio_tick(rt, audio_flag, osr_sleep);
                        return;
                    }
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
                    schedule_audio_tick(rt, audio_flag, osr_sleep);
                });
            }

            schedule_audio_tick(rt_audio, audio_flag, osr_sleep_audio);
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
            Command::OpenTabAt {
                url,
                insert_idx,
                reply,
            } => {
                let rt2 = Arc::clone(rt);
                dispatch_main_async(move || {
                    let result = rt2
                        .lock()
                        .map_err(|_| WebKitCocoaError::WorkerGone)
                        .and_then(|mut r| r.0.open_tab_at(&url, insert_idx));
                    let _ = reply.send(result);
                });
            }
            Command::MoveTab { from, to } => {
                let rt2 = Arc::clone(rt);
                dispatch_main_async(move || {
                    if let Ok(mut r) = rt2.lock() {
                        r.0.move_tab(from, to);
                    }
                });
            }
            Command::ReopenClosed { reply } => {
                let rt2 = Arc::clone(rt);
                let engine_state2 = Arc::clone(_engine_state);
                dispatch_main_async(move || {
                    // Pop the last closed entry from the engine_state closed_stack.
                    let entry = engine_state2
                        .lock()
                        .ok()
                        .and_then(|mut st| st.closed_stack.pop());
                    let result = match entry {
                        None => Ok(None),
                        Some(e) => rt2
                            .lock()
                            .map_err(|_| WebKitCocoaError::WorkerGone)
                            .and_then(|mut r| r.0.open_tab_at(&e.url, e.original_idx))
                            .map(Some),
                    };
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
            Command::StartFind { query, forward } => {
                let rt2 = Arc::clone(rt);
                dispatch_main_async(move || {
                    if let Ok(r) = rt2.lock() {
                        r.0.find_string(&query, forward);
                    }
                });
            }
            Command::StopFind => {
                let rt2 = Arc::clone(rt);
                dispatch_main_async(move || {
                    if let Ok(r) = rt2.lock() {
                        // Empty query clears the find highlight.
                        r.0.find_string("", true);
                    }
                });
            }
            Command::ClipboardRead { reply } => {
                dispatch_main_async(move || {
                    let result = RuntimeState::clipboard_read();
                    let _ = reply.send(result);
                });
            }
            Command::ClipboardWrite { text } => {
                dispatch_main_async(move || {
                    RuntimeState::clipboard_write(&text);
                });
            }
        }
        false
    }
}
