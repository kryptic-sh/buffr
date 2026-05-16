//! Per-tab WPE WebKit state — lives on the worker thread.
//!
//! Phase 2 (FDO) was removed: the wpebackend-fdo exportable never delivers
//! frames on WPE WebKit 2.52 even with the EGL exportable path, because
//! WPE WebKit prefers the new wpe-platform backend whenever
//! `ENABLE_WPE_PLATFORM=ON` and the WebView's `display` property is set.
//!
//! The next iteration wires a custom [`WPEDisplay`] subclass (`BuffrDisplay`)
//! that hands WebKit our worker-thread EGL display + a [`WPEView`] subclass
//! whose `render_buffer` vmethod copies pixels into the shared [`OsrFrame`].
//! Until those subclasses land, `TabEntry::new` returns `None` and tabs
//! fail to open.

use std::ffi::CString;
use std::sync::Arc;
use std::sync::Mutex;

use buffr_engine::{SharedOsrFrame, SharedOsrViewState, TabId};

use super::egl::EglWorker;
use super::ffi::*;
use super::wpe_subclass::{
    BuffrDisplayHandle, ViewCtx, attach_view_ctx, buffr_display_take_last_view,
};
use super::worker::EngineState;

// ── TabInfo (thread-safe snapshot) ───────────────────────────────────────────

/// Snapshot of one tab's state — updated from GLib signal handlers on the
/// worker thread; read from any thread by the engine via `Mutex<EngineState>`.
#[derive(Debug, Clone)]
pub(crate) struct TabInfo {
    pub id: TabId,
    pub url: String,
    pub title: String,
    pub is_loading: bool,
    pub progress: f64,
    pub is_pinned: bool,
}

impl TabInfo {
    pub(crate) fn new(id: TabId, url: &str) -> Self {
        Self {
            id,
            url: url.to_owned(),
            title: url.to_owned(),
            is_loading: false,
            progress: 0.0,
            is_pinned: false,
        }
    }

    pub(crate) fn to_summary(&self) -> buffr_engine::TabSummary {
        buffr_engine::TabSummary {
            id: self.id,
            browser_id: self.id.0 as i32,
            url: self.url.clone(),
            title: self.title.clone(),
            progress: self.progress as f32,
            is_loading: self.is_loading,
            pinned: self.is_pinned,
            private: false,
        }
    }
}

// ── GLib signal helpers (raw FFI without gtk4 dep) ────────────────────────────

// SAFETY: These symbols are provided by GLib (linked via wpe-webkit-2.0).
unsafe extern "C" {
    fn g_signal_handler_disconnect(instance: *mut std::os::raw::c_void, handler_id: u64);
    fn g_object_unref(object: *mut std::os::raw::c_void);
}

/// Per-tab heap context handed to WebKit's signal handlers via `user_data`.
/// Keeps an Arc clone of the shared engine state so handlers running on the
/// worker thread can update URL / title; loading state is tracked via a
/// dedicated lock-free flag (`is_loading_atomic`) so the load-finished
/// signal can never be lost to mutex contention with the main thread.
pub(crate) struct TabSignalCtx {
    tab_id: TabId,
    engine_state: Arc<Mutex<EngineState>>,
    is_loading_atomic: Arc<std::sync::atomic::AtomicBool>,
    /// Per-tab active flag, shared with this tab's `ViewCtx`. Inactive
    /// tabs must not write to the runtime-wide `is_loading_atomic` —
    /// otherwise a background tab finishing a navigation would clear
    /// the splash overlay while the foreground tab is still loading.
    /// TabInfo.is_loading still updates so the tabstrip's per-tab
    /// progress badge stays accurate for hidden tabs.
    is_active: Arc<std::sync::atomic::AtomicBool>,
}

impl TabSignalCtx {
    /// Mutate the per-tab info under `engine_state`. Uses `try_lock` so a
    /// signal handler running on the worker thread never blocks waiting
    /// for the main thread to release the lock — the main thread is
    /// continuously hitting `engine_state` (for `active_tab_live_url`,
    /// `tabs_summary`, etc.) and a contended `lock()` here can park the
    /// worker for long enough that the app feels frozen. A dropped
    /// signal update is recoverable: the next notify::* fires before
    /// the user notices.
    fn with_tab_info<F: FnOnce(&mut TabInfo)>(&self, f: F) {
        let Ok(mut st) = self.engine_state.try_lock() else {
            return;
        };
        if let Some(info) = st.tabs.iter_mut().find(|t| t.id == self.tab_id) {
            f(info);
            st.address_changed = true;
        }
    }
}

/// `load-changed (WebKitWebView*, WebKitLoadEvent)`. Toggles
/// `TabInfo::is_loading` on STARTED / COMMITTED / FINISHED so buffr-app's
/// `is_loading` accessor returns the right thing — the loading animation
/// uses this to decide whether to keep itself on top of the page.
unsafe extern "C" fn on_load_changed(
    web_view: *mut WebKitWebView,
    event: WebKitLoadEvent,
    user_data: *mut std::os::raw::c_void,
) {
    let _ = web_view;
    if user_data.is_null() {
        return;
    }
    // SAFETY: user_data is a `*const TabSignalCtx` we stash via
    // `Arc::into_raw` when connecting the signal; lives until the
    // matching Arc::from_raw fires in drop_tab_signal_ctx.
    let ctx = unsafe { &*(user_data as *const TabSignalCtx) };
    let started = event == WebKitLoadEvent_WEBKIT_LOAD_STARTED
        || event == WebKitLoadEvent_WEBKIT_LOAD_REDIRECTED;
    // Flip false on COMMITTED, not just FINISHED. WEBKIT_LOAD_FINISHED
    // never fires on pages with long-poll XHRs (google.com keeps an
    // open connection for instant-search), so a FINISHED-only gate
    // pins the splash overlay forever. COMMITTED = main resource
    // headers received and first paint is imminent — that's when
    // the user should see the page.
    let revealed = event == WebKitLoadEvent_WEBKIT_LOAD_COMMITTED
        || event == WebKitLoadEvent_WEBKIT_LOAD_FINISHED;
    let finished = event == WebKitLoadEvent_WEBKIT_LOAD_FINISHED;
    use std::sync::atomic::Ordering;
    // Only the active tab drives the runtime-wide splash gate. Background
    // tabs still emit load-changed (LOAD_STARTED on background nav, etc.)
    // but their state lives in TabInfo, not on the shared atomic.
    if ctx.is_active.load(Ordering::Relaxed) {
        if started {
            ctx.is_loading_atomic.store(true, Ordering::SeqCst);
        } else if revealed {
            ctx.is_loading_atomic.store(false, Ordering::SeqCst);
        }
    }
    tracing::debug!(
        ?event,
        started,
        revealed,
        finished,
        "webkit: load-changed signal"
    );
    // Mirror into TabInfo on a best-effort basis for tab-summary readers
    // (progress bar, etc.). Skipping when the main thread holds the lock
    // is fine — the next paint reads is_loading_atomic which is the
    // load-bearing input to the animation gate.
    ctx.with_tab_info(|info| {
        if started {
            info.is_loading = true;
            info.progress = 0.0;
        } else if finished {
            info.is_loading = false;
            info.progress = 1.0;
        } else if revealed {
            // COMMITTED: page begin paint, progress ~50%. Keep
            // is_loading=true so progress bar stays visible until
            // FINISHED, but overlay drops via atomic.
            info.progress = 0.5;
        }
    });
}

/// `notify::uri` — fires whenever WebKit's view URL changes (redirects,
/// SPA pushState, etc.). Mirrors it into `TabInfo.url` so the omnibar
/// stays in sync.
unsafe extern "C" fn on_notify_uri(
    web_view: *mut WebKitWebView,
    _pspec: *mut std::os::raw::c_void,
    user_data: *mut std::os::raw::c_void,
) {
    if user_data.is_null() {
        return;
    }
    // SAFETY: see on_load_changed.
    let ctx = unsafe { &*(user_data as *const TabSignalCtx) };
    // SAFETY: web_view is the signal-emitting object; valid for the call.
    let uri_ptr = unsafe { webkit_web_view_get_uri(web_view) };
    if uri_ptr.is_null() {
        return;
    }
    // SAFETY: WebKit guarantees a null-terminated UTF-8 URI string.
    let uri = unsafe { std::ffi::CStr::from_ptr(uri_ptr) }
        .to_string_lossy()
        .into_owned();
    ctx.with_tab_info(|info| info.url = uri);
}

/// `notify::title` — mirrors WebKit's page title into `TabInfo.title`
/// so tab pills show the document title once it's parsed (instead of
/// the URL placeholder we set on tab creation).
unsafe extern "C" fn on_notify_title(
    web_view: *mut WebKitWebView,
    _pspec: *mut std::os::raw::c_void,
    user_data: *mut std::os::raw::c_void,
) {
    if user_data.is_null() {
        return;
    }
    // SAFETY: see on_load_changed.
    let ctx = unsafe { &*(user_data as *const TabSignalCtx) };
    // SAFETY: web_view is the signal-emitting object; valid for the call.
    let title_ptr = unsafe { webkit_web_view_get_title(web_view) };
    if title_ptr.is_null() {
        return;
    }
    // SAFETY: WebKit guarantees a null-terminated UTF-8 title string.
    let title = unsafe { std::ffi::CStr::from_ptr(title_ptr) }
        .to_string_lossy()
        .into_owned();
    if title.is_empty() {
        return;
    }
    ctx.with_tab_info(|info| info.title = title);
}

/// GLib `GClosureNotify` for the per-connection `Arc<TabSignalCtx>`
/// pointer we leak in TabEntry::new. Each signal connection holds one
/// Arc clone; this fires once per disconnect, decrementing the Arc.
/// The last decrement frees the `TabSignalCtx`.
unsafe extern "C" fn drop_tab_signal_ctx(
    user_data: *mut std::os::raw::c_void,
    _closure: *mut _GClosure,
) {
    if !user_data.is_null() {
        // SAFETY: user_data was produced by Arc::into_raw in TabEntry::new.
        drop(unsafe { Arc::from_raw(user_data as *const TabSignalCtx) });
    }
}

// ── TabEntry ──────────────────────────────────────────────────────────────────

/// One open browser tab. Owns the WebKitWebView GObject. The shared
/// BuffrDisplay lives on `WpeRuntime`; tabs hold a borrowed raw pointer
/// only — the WebView itself retains its own ref on the display, so it
/// stays alive for the WebView's lifetime even if WpeRuntime races a drop.
pub(crate) struct TabEntry {
    pub id: TabId,
    pub web_view: *mut WebKitWebView,
    /// Borrowed WPEView pointer that BuffrDisplay handed to WebKit. Owned
    /// by the WebView; we only keep it for input dispatch / lookup.
    pub wpe_view: *mut WPEView,
    load_changed_id: u64,
    notify_title_id: u64,
    notify_uri_id: u64,
    /// Shared per-tab flag wired into both `ViewCtx` (pixel write gate)
    /// and `TabSignalCtx` (is_loading_atomic write gate). Owned by
    /// `WpeRuntime` so it can flip the active tab via `select_tab`.
    pub is_active: Arc<std::sync::atomic::AtomicBool>,
}

// SAFETY: TabEntry owns C pointers that are used only on the worker thread.
unsafe impl Send for TabEntry {}

impl TabEntry {
    /// Build a WebView through the wpe-platform path: create a BuffrDisplay
    /// (hands WebKit our EGL display + a fake screen), then construct the
    /// WebView with the `display` property. WebKit calls our display's
    /// `create_view` vmethod, which we then attach the per-frame ViewCtx
    /// to so the render callback can find the shared OsrFrame.
    /// Construct a WebView bound to the shared BuffrDisplay owned by
    /// [`WpeRuntime`]. The display ref count is bumped by WebKit (one ref
    /// per WebView via the `display` construct property), and dropped when
    /// the WebView is unreffed — independent of WpeRuntime's own ref.
    pub(crate) fn new(
        id: TabId,
        url: &str,
        display: *mut WPEDisplay,
        frame: SharedOsrFrame,
        view: SharedOsrViewState,
        engine_state: Arc<Mutex<EngineState>>,
        is_loading_atomic: Arc<std::sync::atomic::AtomicBool>,
        is_active: Arc<std::sync::atomic::AtomicBool>,
    ) -> Option<Self> {
        if display.is_null() {
            tracing::error!("webkit: TabEntry::new called with NULL display");
            return None;
        }

        // WebKitWebView via the platform path. WebKit calls our display's
        // create_view vmethod during construction; we recover the view
        // pointer via the stash below.
        let web_view = unsafe {
            let key = CString::new("display").unwrap();
            let view_obj = g_object_new(
                webkit_web_view_get_type(),
                key.as_ptr(),
                display,
                std::ptr::null::<u8>(),
            );
            if view_obj.is_null() {
                tracing::error!("webkit: g_object_new(WebKitWebView, display=…) returned NULL");
                return None;
            }
            view_obj as *mut WebKitWebView
        };

        let wpe_view = unsafe { buffr_display_take_last_view() };
        if wpe_view.is_null() {
            tracing::error!("webkit: BuffrDisplay never called create_view");
            unsafe { g_object_unref(web_view as *mut _) };
            return None;
        }
        attach_view_ctx(
            wpe_view,
            ViewCtx {
                frame,
                view,
                last_ingest_us: std::sync::atomic::AtomicU64::new(0),
                is_active: Arc::clone(&is_active),
            },
        );
        tracing::info!("webkit: ViewCtx attached to WPEView");

        // Connect load-changed / notify::uri / notify::title so the engine
        // state's TabInfo stays in sync. Without this:
        //   - is_loading is set to true on navigate but never cleared, so
        //     the loading animation overlay never deactivates (page hides
        //     behind it for the entirety of the tab's lifetime).
        //   - the omnibar shows the URL we asked WebKit to load, never
        //     the URL it ended up on after redirects / pushState.
        //   - tab pills keep the URL placeholder until they're reopened.
        //
        // Shared per-tab handler context. Each signal connection holds
        // one Arc clone (leaked into a raw pointer) and the matching
        // GClosureNotify drops it on disconnect. Using Arc instead of a
        // single Box prevents a double-free: 3 signals × 1 destroy_notify
        // each = 3 drops, which on a Box would tear up the same allocation
        // three times.
        let ctx = Arc::new(TabSignalCtx {
            tab_id: id,
            engine_state,
            is_loading_atomic,
            is_active: Arc::clone(&is_active),
        });
        let connect = |signal: &str, cb: unsafe extern "C" fn()| -> u64 {
            let signal_c = CString::new(signal).unwrap();
            // Hand WebKit a per-connection Arc clone via Arc::into_raw;
            // GLib calls drop_tab_signal_ctx on disconnect which
            // reconstitutes + drops it.
            let arc_clone = Arc::clone(&ctx);
            let user_data = Arc::into_raw(arc_clone) as *mut std::os::raw::c_void;
            // SAFETY: web_view is a live WebKitWebView; signal_c lives
            // until the end of this block; cb is a 'static C fn pointer;
            // user_data is leaked + freed via drop_tab_signal_ctx.
            unsafe {
                g_signal_connect_data(
                    web_view as *mut _,
                    signal_c.as_ptr(),
                    Some(cb),
                    user_data,
                    Some(drop_tab_signal_ctx),
                    0,
                )
            }
        };
        let load_changed_id = connect("load-changed", unsafe {
            // SAFETY: transmute matches the fn pointer ABI shape required
            // by g_signal_connect_data (a callable C fn). The actual
            // arity-correct signature is enforced when WebKit invokes it.
            std::mem::transmute::<
                unsafe extern "C" fn(*mut WebKitWebView, WebKitLoadEvent, *mut std::os::raw::c_void),
                unsafe extern "C" fn(),
            >(on_load_changed)
        });
        let notify_uri_id = connect("notify::uri", unsafe {
            std::mem::transmute::<
                unsafe extern "C" fn(
                    *mut WebKitWebView,
                    *mut std::os::raw::c_void,
                    *mut std::os::raw::c_void,
                ),
                unsafe extern "C" fn(),
            >(on_notify_uri)
        });
        let notify_title_id = connect("notify::title", unsafe {
            std::mem::transmute::<
                unsafe extern "C" fn(
                    *mut WebKitWebView,
                    *mut std::os::raw::c_void,
                    *mut std::os::raw::c_void,
                ),
                unsafe extern "C" fn(),
            >(on_notify_title)
        });

        let url_c = CString::new(url).unwrap_or_default();
        unsafe { webkit_web_view_load_uri(web_view, url_c.as_ptr()) };
        tracing::info!("webkit: created WebView id={id:?} url={url}");

        Some(TabEntry {
            id,
            web_view,
            wpe_view,
            load_changed_id,
            notify_title_id,
            notify_uri_id,
            is_active,
        })
    }

    /// Navigate to a new URL.
    pub(crate) fn load_uri(&self, url: &str) {
        let c = CString::new(url).unwrap_or_default();
        // SAFETY: web_view is valid for the tab's lifetime; c is null-terminated.
        unsafe { webkit_web_view_load_uri(self.web_view, c.as_ptr()) };
    }

    /// Resize the toplevel that owns this tab's view. WebKit propagates the
    /// new size through to the WebProcess via our resize_vfunc → resized →
    /// view_resized chain.
    pub(crate) fn resize(&self, width: u32, height: u32) {
        if self.wpe_view.is_null() {
            return;
        }
        unsafe {
            let tl = wpe_view_get_toplevel(self.wpe_view);
            if !tl.is_null() {
                wpe_toplevel_resize(tl, width as i32, height as i32);
            }
        }
    }

    /// Current URL from the WebView (may lag by one GLib tick).
    pub(crate) fn current_url(&self) -> String {
        // SAFETY: web_view is valid for the tab's lifetime.
        let ptr = unsafe { webkit_web_view_get_uri(self.web_view) };
        if ptr.is_null() {
            String::new()
        } else {
            // SAFETY: ptr is a valid null-terminated UTF-8 string.
            unsafe { std::ffi::CStr::from_ptr(ptr) }
                .to_string_lossy()
                .into_owned()
        }
    }

    /// Whether the tab is currently playing audio.
    pub(crate) fn is_playing_audio(&self) -> bool {
        // SAFETY: web_view is valid for the tab's lifetime.
        unsafe { webkit_web_view_is_playing_audio(self.web_view) != 0 }
    }
}

impl Drop for TabEntry {
    fn drop(&mut self) {
        // SAFETY: GLib owns the signal lifetime; disconnect before unref to
        // prevent callbacks firing on a freed object.
        unsafe {
            if self.load_changed_id != 0 {
                g_signal_handler_disconnect(self.web_view as *mut _, self.load_changed_id);
            }
            if self.notify_title_id != 0 {
                g_signal_handler_disconnect(self.web_view as *mut _, self.notify_title_id);
            }
            if self.notify_uri_id != 0 {
                g_signal_handler_disconnect(self.web_view as *mut _, self.notify_uri_id);
            }
            g_object_unref(self.web_view as *mut _);
        }
    }
}

// ── WpeRuntime ────────────────────────────────────────────────────────────────

/// Top-level runtime that lives on the GLib worker thread. Owns the active
/// tab(s), the worker-thread EGL display, and the single shared
/// BuffrDisplay that every WebView is constructed against.
pub(crate) struct WpeRuntime {
    /// All open tabs in strip order. Mirrors `EngineState.tabs` and uses
    /// the same `TabId` for cross-referencing. At most one is "active"
    /// (its `is_active` flag is true) — that's the tab whose paints
    /// reach the shared `OsrFrame` and whose load-state drives the
    /// runtime-wide splash gate.
    pub tabs: Vec<TabEntry>,
    /// Index into `tabs` of the active entry. `None` only between
    /// `close_active` and the next `open_tab`.
    pub active_idx: Option<usize>,
    pub engine_state: Arc<Mutex<EngineState>>,
    pub frame: SharedOsrFrame,
    pub view: SharedOsrViewState,
    pub egl: EglWorker,
    /// Shared display. Lives for the runtime's lifetime; each WebView
    /// bumps its ref via the `display` construct property.
    display: BuffrDisplayHandle,
    /// Cross-thread flag for the active tab's load state. The
    /// load-changed signal handler stores into this; `WebKitEngine`'s
    /// `BrowserEngine::is_loading` impl reads it. Kept outside the
    /// engine_state mutex so worker-thread signal handlers never race
    /// (or worse, drop the update) against main-thread reads.
    pub is_loading_atomic: Arc<std::sync::atomic::AtomicBool>,
}

impl WpeRuntime {
    pub(crate) fn new(
        frame: SharedOsrFrame,
        view: SharedOsrViewState,
        engine_state: Arc<Mutex<EngineState>>,
        egl: EglWorker,
        is_loading_atomic: Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<Self, String> {
        let (width, height, hz) = {
            let st = engine_state
                .lock()
                .map_err(|e| format!("mutex poison: {e}"))?;
            (
                st.width,
                st.height,
                view.frame_rate_hz
                    .load(std::sync::atomic::Ordering::Relaxed)
                    .max(1),
            )
        };

        let display =
            BuffrDisplayHandle::new(egl.raw_display(), width, height, 1.0, hz).ok_or_else(|| {
                "BuffrDisplayHandle::new returned None".to_string()
            })?;

        // Connect + publish as primary once. wpe_display_get_primary calls
        // inside WebKit now resolve here for the whole runtime lifetime.
        unsafe {
            let mut error: *mut GError = std::ptr::null_mut();
            let ok = wpe_display_connect(display.raw, &mut error);
            if ok == 0 {
                return Err("wpe_display_connect failed".into());
            }
            wpe_display_set_primary(display.raw);
        }
        tracing::info!("webkit: BuffrDisplay created + connected (shared)");

        Ok(Self {
            tabs: Vec::new(),
            active_idx: None,
            engine_state,
            frame,
            view,
            egl,
            display,
            is_loading_atomic,
        })
    }

    /// The currently active tab, if any.
    fn active_tab(&self) -> Option<&TabEntry> {
        self.active_idx.and_then(|i| self.tabs.get(i))
    }

    pub(crate) fn open_tab(&mut self, url: &str) -> Result<TabId, String> {
        let id = {
            let mut st = self
                .engine_state
                .lock()
                .map_err(|e| format!("mutex poison: {e}"))?;
            let id = TabId(st.next_id);
            st.next_id += 1;
            id
        };

        // Deactivate the current active tab BEFORE creating the new
        // WebView. WebKit emits LOAD_STARTED + initial paint events
        // synchronously from webkit_web_view_load_uri inside
        // TabEntry::new — if the previous tab is still flagged active,
        // its ViewCtx would still write pixels into the shared frame
        // and its TabSignalCtx would still clobber is_loading_atomic
        // while we're trying to switch.
        if let Some(prev) = self.active_tab() {
            prev.is_active
                .store(false, std::sync::atomic::Ordering::SeqCst);
        }

        // TabInfo must exist before signal handlers fire — load-changed
        // can race the return from TabEntry::new because webkit_web_view_load_uri
        // emits LOAD_STARTED synchronously.
        let info = TabInfo::new(id, url);
        if let Ok(mut st) = self.engine_state.lock() {
            st.tabs.push(info);
            st.active_idx = Some(st.tabs.len() - 1);
        }

        // Reset the loading flag here on the worker thread so the
        // signal handler doesn't have to win the race against the main
        // thread observing is_loading=false from a previous tab. Also
        // force the renderer to wait for the new tab's first paint
        // instead of compositing the previous tab's last frame.
        self.is_loading_atomic
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if let Ok(mut frame) = self.frame.lock() {
            frame.needs_fresh = true;
        }

        let is_active = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let entry = TabEntry::new(
            id,
            url,
            self.display.raw,
            Arc::clone(&self.frame),
            Arc::clone(&self.view),
            Arc::clone(&self.engine_state),
            Arc::clone(&self.is_loading_atomic),
            is_active,
        )
        .ok_or_else(|| "TabEntry::new returned None".to_string())?;

        self.tabs.push(entry);
        self.active_idx = Some(self.tabs.len() - 1);
        Ok(id)
    }

    /// Switch the active tab to the one with `id`. No-op if `id` isn't
    /// open or is already active. On switch, the previous active tab's
    /// `is_active` flag flips to false (its paints stop reaching the
    /// shared frame); the new active tab's flag flips to true and
    /// `frame.needs_fresh=true` makes the renderer wait for its first
    /// paint instead of compositing the previous tab's last frame.
    pub(crate) fn select_tab(&mut self, id: TabId) -> bool {
        let Some(new_idx) = self.tabs.iter().position(|t| t.id == id) else {
            return false;
        };
        if self.active_idx == Some(new_idx) {
            return false;
        }
        if let Some(prev) = self.active_tab() {
            prev.is_active
                .store(false, std::sync::atomic::Ordering::SeqCst);
        }
        self.tabs[new_idx]
            .is_active
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.active_idx = Some(new_idx);
        if let Ok(mut st) = self.engine_state.lock() {
            st.active_idx = Some(new_idx);
        }
        // Re-snapshot the runtime-wide is_loading from the newly active
        // tab's TabInfo so the splash gate reflects the new tab, not the
        // one we just left. Bump needs_fresh so the renderer holds the
        // animation until the new view paints.
        let new_is_loading = self
            .engine_state
            .lock()
            .ok()
            .and_then(|st| st.tabs.get(new_idx).map(|t| t.is_loading))
            .unwrap_or(false);
        self.is_loading_atomic
            .store(new_is_loading, std::sync::atomic::Ordering::SeqCst);
        if let Ok(mut frame) = self.frame.lock() {
            frame.needs_fresh = true;
        }
        tracing::info!(?id, new_idx, "webkit: select_tab");
        true
    }

    /// Close the active tab. Returns true if a tab was actually closed.
    /// Drops the WebView (which releases its display ref) and reassigns
    /// active to the previous tab (or none if this was the last).
    pub(crate) fn close_active(&mut self) -> bool {
        let Some(idx) = self.active_idx else {
            return false;
        };
        if idx >= self.tabs.len() {
            return false;
        }
        // Drop the WebView (disconnects signals + g_object_unref).
        let _ = self.tabs.remove(idx);

        // Pick a new active: prefer the tab now at the same index
        // (the one that used to be after us); if we removed the last,
        // fall back to the new last.
        let new_idx = if self.tabs.is_empty() {
            None
        } else if idx < self.tabs.len() {
            Some(idx)
        } else {
            Some(self.tabs.len() - 1)
        };
        self.active_idx = new_idx;
        if let Some(i) = new_idx {
            self.tabs[i]
                .is_active
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }

        if let Ok(mut st) = self.engine_state.lock() {
            if idx < st.tabs.len() {
                st.tabs.remove(idx);
            }
            st.active_idx = new_idx;
        }
        if let Ok(mut frame) = self.frame.lock() {
            frame.needs_fresh = true;
        }
        true
    }

    pub(crate) fn navigate(&mut self, url: &str) {
        if let Some(tab) = self.active_tab() {
            tab.load_uri(url);
        }
        if let Ok(mut st) = self.engine_state.lock() {
            if let Some(idx) = st.active_idx {
                if let Some(tab_info) = st.tabs.get_mut(idx) {
                    tab_info.url = url.to_owned();
                    tab_info.is_loading = true;
                }
            }
        }
    }

    pub(crate) fn resize(&mut self, width: u32, height: u32) {
        if let Ok(mut st) = self.engine_state.lock() {
            st.width = width;
            st.height = height;
        }
        // Resize every tab's toplevel — hidden tabs need correct dims
        // so when they're activated their next paint matches the
        // shared frame's size (otherwise is_osr_frame_fresh rejects).
        for tab in &self.tabs {
            tab.resize(width, height);
        }
        if let Ok(mut frame) = self.frame.lock() {
            let new_len = (width * height * 4) as usize;
            frame.width = width;
            frame.height = height;
            frame.pixels.resize(new_len, 0);
            frame.needs_fresh = true;
        }
    }

    /// Monotonic millisecond clock used for `wpe_event_*_new` time stamps.
    /// WebKit reads it back via `wpe_event_get_time`; only the relative
    /// ordering matters, so a process-start epoch is fine.
    fn event_time_ms() -> u32 {
        static ONCE: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        let epoch = *ONCE.get_or_init(std::time::Instant::now);
        epoch.elapsed().as_millis() as u32
    }

    /// Construct and dispatch a `WPEEvent` for this runtime's active tab.
    /// `make` is invoked with the live `WPEView*` and the current time and
    /// must return a freshly-owned event (`wpe_event_*_new` already adds
    /// one ref; we unref after `wpe_view_event` takes its own).
    fn dispatch_event<F>(&self, make: F)
    where
        F: FnOnce(*mut WPEView, u32) -> *mut WPEEvent,
    {
        let Some(tab) = self.active_tab() else {
            return;
        };
        let view = tab.wpe_view;
        if view.is_null() {
            return;
        }
        let time = Self::event_time_ms();
        // SAFETY: view is a live WPEView held by TabEntry; the factory
        // closure receives it for the duration of this call. Both new()
        // and wpe_view_event are thread-bound to the GLib main loop —
        // dispatch_* always runs on the worker thread.
        let event = make(view, time);
        if event.is_null() {
            return;
        }
        unsafe {
            wpe_view_event(view, event);
            wpe_event_unref(event);
        }
    }

    /// Run `script` in the active tab's main JavaScript world. Fire and
    /// forget: WebKit posts the call to its UI process and returns
    /// immediately; the result (if any) lands on an async callback that
    /// we don't currently consume. Used by `WebKitEngine::dispatch` to
    /// implement vim-style scrolling and any other catch-all action.
    pub(crate) fn eval_js(&self, script: &str) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        if tab.web_view.is_null() {
            return;
        }
        let Ok(script_c) = CString::new(script) else {
            tracing::warn!("webkit: eval_js: script contained NUL byte");
            return;
        };
        // SAFETY: web_view is held by TabEntry for the tab's lifetime;
        // script_c lives until the end of this call (length=-1 means
        // strlen, so WebKit reads up to the NUL terminator). All other
        // ptrs are NULL (default JS world, no source URI, no callback).
        unsafe {
            webkit_web_view_evaluate_javascript(
                tab.web_view,
                script_c.as_ptr(),
                -1,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null_mut(),
                None,
                std::ptr::null_mut(),
            );
        }
    }

    pub(crate) fn dispatch_keyboard(&self, key_code: u32, pressed: bool, modifiers: u32) {
        let event_type = if pressed {
            WPEEventType_WPE_EVENT_KEYBOARD_KEY_DOWN
        } else {
            WPEEventType_WPE_EVENT_KEYBOARD_KEY_UP
        };
        self.dispatch_event(|view, time| unsafe {
            wpe_event_keyboard_new(
                event_type,
                view,
                WPEInputSource_WPE_INPUT_SOURCE_KEYBOARD,
                time,
                modifiers,
                // `keycode` is the hardware code; `keyval` is the XKB
                // keysym. For Phase-2 buffr passes a single value
                // through `key_code`; route it to both so chrome key
                // handling reads either field correctly.
                key_code,
                key_code,
            )
        });
    }

    pub(crate) fn dispatch_pointer_motion(&self, x: i32, y: i32, modifiers: u32) {
        self.dispatch_event(|view, time| unsafe {
            wpe_event_pointer_move_new(
                WPEEventType_WPE_EVENT_POINTER_MOVE,
                view,
                WPEInputSource_WPE_INPUT_SOURCE_MOUSE,
                time,
                modifiers,
                x as f64,
                y as f64,
                // delta_x/delta_y are the relative motion since the last
                // POINTER_MOVE; buffr-app sends absolute positions so we
                // don't carry per-tab deltas yet. WebKit copes fine — it
                // primarily uses x/y for hit-testing.
                0.0,
                0.0,
            )
        });
    }

    pub(crate) fn dispatch_pointer_button(
        &self,
        x: i32,
        y: i32,
        button: u32,
        pressed: bool,
        modifiers: u32,
    ) {
        let event_type = if pressed {
            WPEEventType_WPE_EVENT_POINTER_DOWN
        } else {
            WPEEventType_WPE_EVENT_POINTER_UP
        };
        self.dispatch_event(|view, time| unsafe {
            wpe_event_pointer_button_new(
                event_type,
                view,
                WPEInputSource_WPE_INPUT_SOURCE_MOUSE,
                time,
                modifiers,
                button,
                x as f64,
                y as f64,
                // Always reporting `1` (single press) is OK for now;
                // double-click detection is the chrome layer's job.
                1,
            )
        });
    }

    pub(crate) fn dispatch_axis(
        &self,
        x: i32,
        y: i32,
        delta_x: i32,
        delta_y: i32,
        modifiers: u32,
    ) {
        // Pure horizontal+vertical zero → treat as "scroll stop" so WebKit
        // can release momentum state.
        let is_stop = (delta_x == 0 && delta_y == 0) as gboolean;
        self.dispatch_event(|view, time| unsafe {
            wpe_event_scroll_new(
                view,
                WPEInputSource_WPE_INPUT_SOURCE_MOUSE,
                time,
                modifiers,
                delta_x as f64,
                delta_y as f64,
                // `precise_deltas = TRUE` for trackpad-style smooth
                // scrolling. CEF input is already in pixel units, so this
                // matches the scale WebKit's smooth-scroll code expects.
                1,
                is_stop,
                x as f64,
                y as f64,
            )
        });
    }

    pub(crate) fn any_audio_active(&self) -> bool {
        self.tabs.iter().any(|t| t.is_playing_audio())
    }
}
