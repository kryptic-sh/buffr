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

use std::sync::Mutex;
use std::sync::Arc;

use buffr_engine::{SharedOsrFrame, SharedOsrViewState, TabId};

use super::ffi::*;
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

/// One open browser tab. Placeholder until the BuffrDisplay/BuffrView
/// subclass machinery lands.
pub(crate) struct TabEntry {
    pub id: TabId,
    pub web_view: *mut WebKitWebView,
    load_changed_id: u64,
    notify_title_id: u64,
    notify_uri_id: u64,
}

// SAFETY: TabEntry owns C pointers that are used only on the worker thread.
unsafe impl Send for TabEntry {}

impl TabEntry {
    /// Stub: returns `None` until the wpe-platform display + view subclasses
    /// are wired. The full body lands in a follow-up commit.
    pub(crate) fn new(
        _id: TabId,
        _url: &str,
        _width: u32,
        _height: u32,
        _frame: SharedOsrFrame,
        _view: SharedOsrViewState,
        _engine_state: Arc<Mutex<EngineState>>,
    ) -> Option<Self> {
        tracing::warn!(
            "webkit: TabEntry::new is a stub — wpe-platform subclasses not wired yet"
        );
        None
    }

    /// Navigate to a new URL. No-op until [`Self::new`] is real.
    pub(crate) fn load_uri(&self, url: &str) {
        let _ = url;
    }

    /// Resize. No-op until the platform path is wired.
    pub(crate) fn resize(&self, _width: u32, _height: u32) {}

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
/// tab(s) once tab creation comes online.
pub(crate) struct WpeRuntime {
    pub tab: Option<TabEntry>,
    pub engine_state: Arc<Mutex<EngineState>>,
    pub frame: SharedOsrFrame,
    pub view: SharedOsrViewState,
}

impl WpeRuntime {
    pub(crate) fn new(
        frame: SharedOsrFrame,
        view: SharedOsrViewState,
        engine_state: Arc<Mutex<EngineState>>,
    ) -> Self {
        Self {
            tab: None,
            engine_state,
            frame,
            view,
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
            Arc::clone(&self.engine_state),
        )
        .ok_or_else(|| "TabEntry::new returned None (wpe-platform subclasses pending)".to_string())?;

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
