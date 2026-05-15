//! Per-tab WebView lifecycle — lives on the GTK main thread.
//!
//! `TabEntry` owns a `webkit6::WebView` and wires its signals back into the
//! shared `Arc<Mutex<EngineState>>` so the engine thread can read URL / title /
//! load state without blocking.
//!
//! # Thread safety
//!
//! `TabEntry` is `!Send` because `WebView` is a GTK object. All `TabEntry`
//! instances are owned by `GtkRuntime` which itself runs exclusively on the
//! dedicated GTK thread.

use std::sync::{Arc, Mutex};

use webkit6::prelude::*;
use webkit6::{LoadEvent, WebView};

use buffr_engine::TabId;

use super::worker::EngineState;

// ── TabEntry ──────────────────────────────────────────────────────────────────

/// One open browser tab. Owns a `WebView` and its signal handler IDs.
pub(crate) struct TabEntry {
    pub id: TabId,
    pub web_view: WebView,
}

impl TabEntry {
    /// Create a new WebView, connect signals, and load `url`.
    pub(crate) fn new(
        id: TabId,
        url: &str,
        _width: u32,
        _height: u32,
        engine_state: Arc<Mutex<EngineState>>,
    ) -> Self {
        let web_view = WebView::new();

        // ── load-changed signal ──────────────────────────────────────────────
        {
            let st = Arc::clone(&engine_state);
            web_view.connect_load_changed(move |wv, event| {
                let url = wv.uri().map(|s| s.to_string()).unwrap_or_default();
                let title = wv.title().map(|s| s.to_string()).unwrap_or_default();
                let is_loading = !matches!(event, LoadEvent::Finished);
                if let Ok(mut guard) = st.lock()
                    && let Some(tab) = guard.tabs.iter_mut().find(|t| t.id == id)
                {
                    tab.url = url;
                    tab.title = title;
                    tab.is_loading = is_loading;
                    tab.can_go_back = wv.can_go_back();
                    tab.can_go_forward = wv.can_go_forward();
                }
            });
        }

        // ── title::notify signal ─────────────────────────────────────────────
        {
            let st = Arc::clone(&engine_state);
            web_view.connect_title_notify(move |wv| {
                let title = wv.title().map(|s| s.to_string()).unwrap_or_default();
                if let Ok(mut guard) = st.lock()
                    && let Some(tab) = guard.tabs.iter_mut().find(|t| t.id == id)
                {
                    tab.title = title;
                }
            });
        }

        // ── uri::notify signal ───────────────────────────────────────────────
        {
            let st = Arc::clone(&engine_state);
            web_view.connect_uri_notify(move |wv| {
                let url = wv.uri().map(|s| s.to_string()).unwrap_or_default();
                if let Ok(mut guard) = st.lock()
                    && let Some(tab) = guard.tabs.iter_mut().find(|t| t.id == id)
                {
                    tab.url = url;
                    tab.can_go_back = wv.can_go_back();
                    tab.can_go_forward = wv.can_go_forward();
                }
            });
        }

        // ── estimated-load-progress::notify signal ───────────────────────────
        {
            let st = Arc::clone(&engine_state);
            web_view.connect_estimated_load_progress_notify(move |wv| {
                let progress = wv.estimated_load_progress();
                if let Ok(mut guard) = st.lock()
                    && let Some(tab) = guard.tabs.iter_mut().find(|t| t.id == id)
                {
                    tab.progress = progress;
                }
            });
        }

        // Mirror initial tab info into engine state.
        {
            if let Ok(mut guard) = engine_state.lock() {
                guard.tabs.push(super::worker::TabInfo {
                    id,
                    url: url.to_owned(),
                    title: String::new(),
                    is_loading: true,
                    can_go_back: false,
                    can_go_forward: false,
                    progress: 0.0,
                });
            }
        }

        // Start loading the initial URL.
        web_view.load_uri(url);

        tracing::info!("webkitgtk runtime: opened tab {id:?} → {url}");
        TabEntry { id, web_view }
    }

    /// Navigate to a new URL.
    pub(crate) fn load_uri(&self, url: &str) {
        self.web_view.load_uri(url);
    }

    /// Go back one step in the navigation history.
    pub(crate) fn go_back(&self) {
        self.web_view.go_back();
    }

    /// Go forward one step in the navigation history.
    pub(crate) fn go_forward(&self) {
        self.web_view.go_forward();
    }

    /// Reload the current page.
    pub(crate) fn reload(&self) {
        self.web_view.reload();
    }

    /// Stop the current load.
    pub(crate) fn stop(&self) {
        self.web_view.stop_loading();
    }

    /// Whether the back-stack is non-empty.
    pub(crate) fn can_go_back(&self) -> bool {
        self.web_view.can_go_back()
    }

    /// Whether the forward-stack is non-empty.
    pub(crate) fn can_go_forward(&self) -> bool {
        self.web_view.can_go_forward()
    }
}
