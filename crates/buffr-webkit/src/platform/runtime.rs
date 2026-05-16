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

/// One open browser tab. Owns the WebKitWebView GObject + the BuffrDisplay
/// it was constructed against. Drop order: disconnect signals → unref view
/// → drop BuffrDisplayHandle.
pub(crate) struct TabEntry {
    #[allow(dead_code)]
    pub id: TabId,
    pub web_view: *mut WebKitWebView,
    #[allow(dead_code)]
    display: BuffrDisplayHandle,
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
    pub(crate) fn new(
        id: TabId,
        url: &str,
        width: u32,
        height: u32,
        frame: SharedOsrFrame,
        view: SharedOsrViewState,
        egl: &EglWorker,
        _engine_state: Arc<Mutex<EngineState>>,
    ) -> Option<Self> {
        // 1. BuffrDisplay: hands WebKit our EGL display + viewport.
        let display = BuffrDisplayHandle::new(egl.raw_display(), width, height, 1.0, 60)?;

        // Explicitly connect — our vmethod returns TRUE no-op, but WebKit's
        // internal state machine still expects `wpe_display_connect` to
        // have been called before WebView creation. Also publish it as
        // primary so any WebKit code path that calls wpe_display_get_primary
        // resolves to ours instead of falling back to a registered backend.
        unsafe {
            let mut error: *mut GError = std::ptr::null_mut();
            let ok = wpe_display_connect(display.raw, &mut error);
            if ok == 0 {
                tracing::error!("webkit: wpe_display_connect failed");
                return None;
            }
            wpe_display_set_primary(display.raw);
        }
        tracing::info!("webkit: BuffrDisplay created + connected");

        // 2. WebKitWebView via the platform path (no backend property).
        let web_view = unsafe {
            let key = CString::new("display").unwrap();
            let raw_display = display.raw;
            // g_object_new with one ("display", BuffrDisplay*) construct
            // property, NULL-terminated. Cast to WebKitWebView*.
            let view = g_object_new(
                webkit_web_view_get_type(),
                key.as_ptr(),
                raw_display,
                std::ptr::null::<u8>(),
            );
            if view.is_null() {
                tracing::error!("webkit: g_object_new(WebKitWebView, display=…) returned NULL");
                return None;
            }
            view as *mut WebKitWebView
        };

        // 3. Recover the WPEView our display just created so we can attach
        // the ViewCtx with the shared OsrFrame.
        let wpe_view = unsafe { buffr_display_take_last_view() };
        if wpe_view.is_null() {
            tracing::error!("webkit: BuffrDisplay never called create_view");
            unsafe { g_object_unref(web_view as *mut _) };
            return None;
        }
        attach_view_ctx(wpe_view, ViewCtx { frame, view });
        tracing::info!("webkit: ViewCtx attached to WPEView");

        // 4. Load initial URL.
        let url_c = CString::new(url).unwrap_or_default();
        unsafe { webkit_web_view_load_uri(web_view, url_c.as_ptr()) };
        tracing::info!("webkit: created WebView id={id:?} url={url}");

        Some(TabEntry {
            id,
            web_view,
            display,
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
/// tab(s) plus the worker-thread EGL display that BuffrDisplay hands WebKit.
pub(crate) struct WpeRuntime {
    pub tab: Option<TabEntry>,
    pub engine_state: Arc<Mutex<EngineState>>,
    pub frame: SharedOsrFrame,
    pub view: SharedOsrViewState,
    pub egl: EglWorker,
}

impl WpeRuntime {
    pub(crate) fn new(
        frame: SharedOsrFrame,
        view: SharedOsrViewState,
        engine_state: Arc<Mutex<EngineState>>,
        egl: EglWorker,
    ) -> Self {
        Self {
            tab: None,
            engine_state,
            frame,
            view,
            egl,
        }
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

        let (width, height) = {
            let st = self
                .engine_state
                .lock()
                .map_err(|e| format!("mutex: {e}"))?;
            (st.width, st.height)
        };

        self.tab = None;

        let entry = TabEntry::new(
            id,
            url,
            width,
            height,
            Arc::clone(&self.frame),
            Arc::clone(&self.view),
            &self.egl,
            Arc::clone(&self.engine_state),
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

    pub(crate) fn dispatch_keyboard(&self, _ev_key_code: u32, _ev_pressed: bool, _ev_modifiers: u32) {
        // Stub: input dispatch reattaches once WPEView is wired.
    }

    pub(crate) fn dispatch_pointer_motion(&self, _x: i32, _y: i32, _modifiers: u32) {}

    pub(crate) fn dispatch_pointer_button(
        &self,
        _x: i32,
        _y: i32,
        _button: u32,
        _pressed: bool,
        _modifiers: u32,
    ) {
    }

    pub(crate) fn dispatch_axis(
        &self,
        _x: i32,
        _y: i32,
        _delta_x: i32,
        _delta_y: i32,
        _modifiers: u32,
    ) {
    }

    pub(crate) fn any_audio_active(&self) -> bool {
        self.tab.as_ref().map(|t| t.is_playing_audio()).unwrap_or(false)
    }
}
