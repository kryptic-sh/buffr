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

// ── TabEntry ──────────────────────────────────────────────────────────────────

/// One open browser tab. Owns the WebKitWebView GObject. The shared
/// BuffrDisplay lives on `WpeRuntime`; tabs hold a borrowed raw pointer
/// only — the WebView itself retains its own ref on the display, so it
/// stays alive for the WebView's lifetime even if WpeRuntime races a drop.
pub(crate) struct TabEntry {
    #[allow(dead_code)]
    pub id: TabId,
    pub web_view: *mut WebKitWebView,
    /// Borrowed WPEView pointer that BuffrDisplay handed to WebKit. Owned
    /// by the WebView; we only keep it for input dispatch / lookup.
    pub wpe_view: *mut WPEView,
    load_changed_id: u64,
    notify_title_id: u64,
    notify_uri_id: u64,
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
            },
        );
        tracing::info!("webkit: ViewCtx attached to WPEView");

        let url_c = CString::new(url).unwrap_or_default();
        unsafe { webkit_web_view_load_uri(web_view, url_c.as_ptr()) };
        tracing::info!("webkit: created WebView id={id:?} url={url}");

        Some(TabEntry {
            id,
            web_view,
            wpe_view,
            load_changed_id: 0,
            notify_title_id: 0,
            notify_uri_id: 0,
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
    pub tab: Option<TabEntry>,
    pub engine_state: Arc<Mutex<EngineState>>,
    pub frame: SharedOsrFrame,
    pub view: SharedOsrViewState,
    pub egl: EglWorker,
    /// Shared display. Lives for the runtime's lifetime; each WebView
    /// bumps its ref via the `display` construct property.
    display: BuffrDisplayHandle,
}

impl WpeRuntime {
    pub(crate) fn new(
        frame: SharedOsrFrame,
        view: SharedOsrViewState,
        engine_state: Arc<Mutex<EngineState>>,
        egl: EglWorker,
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
            tab: None,
            engine_state,
            frame,
            view,
            egl,
            display,
        })
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

        self.tab = None;

        let entry = TabEntry::new(
            id,
            url,
            self.display.raw,
            Arc::clone(&self.frame),
            Arc::clone(&self.view),
        )
        .ok_or_else(|| "TabEntry::new returned None".to_string())?;

        let info = TabInfo::new(id, url);
        if let Ok(mut st) = self.engine_state.lock() {
            st.tabs.clear();
            st.tabs.push(info);
            st.active_idx = Some(0);
        }

        self.tab = Some(entry);
        Ok(id)
    }

    /// Close the active tab. Returns true if a tab was actually closed.
    /// Drops the WebView (which releases its display ref) and clears the
    /// tab list on the shared engine_state.
    pub(crate) fn close_active(&mut self) -> bool {
        if self.tab.take().is_none() {
            return false;
        }
        if let Ok(mut st) = self.engine_state.lock() {
            st.tabs.clear();
            st.active_idx = None;
        }
        true
    }

    pub(crate) fn navigate(&mut self, url: &str) {
        if let Some(tab) = &self.tab {
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
        if let Some(tab) = &self.tab {
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
        let Some(tab) = &self.tab else {
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
        self.tab.as_ref().map(|t| t.is_playing_audio()).unwrap_or(false)
    }
}
