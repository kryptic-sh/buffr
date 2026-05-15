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

use std::cell::Cell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use webkit6::prelude::*;
use webkit6::{LoadEvent, WebView};

use buffr_engine::{FaviconUpdate, SharedOsrFrame, SharedOsrViewState, TabId};

use super::osr::request_snapshot;
use super::worker::EngineState;

// ── TabEntry ──────────────────────────────────────────────────────────────────

/// OSR handles passed to `TabEntry::new` so that navigation signals can
/// trigger a real pixel snapshot without exceeding the 7-argument limit.
pub(crate) struct OsrHandles {
    pub frame: SharedOsrFrame,
    pub view: SharedOsrViewState,
    pub snapshot_in_flight: Rc<Cell<bool>>,
}

/// One open browser tab. Owns a `WebView` and its signal handler IDs.
pub(crate) struct TabEntry {
    pub id: TabId,
    pub web_view: WebView,
}

impl TabEntry {
    /// Create a new WebView, connect signals, and load `url`.
    ///
    /// `osr` carries the frame/view/in_flight handles shared with the snapshot
    /// pipeline so that the `load-changed` signal can trigger a real pixel
    /// readback when a navigation completes.
    pub(crate) fn new(
        id: TabId,
        url: &str,
        _width: u32,
        _height: u32,
        engine_state: Arc<Mutex<EngineState>>,
        osr: OsrHandles,
    ) -> Self {
        let web_view = WebView::new();

        // ── Enable developer extras (required for WebInspector::show) ─────────
        {
            if let Some(settings) = webkit6::prelude::WebViewExt::settings(&web_view) {
                settings.set_enable_developer_extras(true);
            }
        }

        // ── load-changed signal ──────────────────────────────────────────────
        {
            let st = Arc::clone(&engine_state);
            let frame_lc = Arc::clone(&osr.frame);
            let view_lc = Arc::clone(&osr.view);
            let in_flight_lc = Rc::clone(&osr.snapshot_in_flight);
            web_view.connect_load_changed(move |wv, event| {
                let url = wv.uri().map(|s| s.to_string()).unwrap_or_default();
                let title = wv.title().map(|s| s.to_string()).unwrap_or_default();
                let is_loading = !matches!(event, LoadEvent::Finished);
                if let Ok(mut guard) = st.lock() {
                    if let Some(tab) = guard.tabs.iter_mut().find(|t| t.id == id) {
                        tab.url = url;
                        tab.title = title;
                        tab.is_loading = is_loading;
                        tab.can_go_back = wv.can_go_back();
                        tab.can_go_forward = wv.can_go_forward();
                    }
                    // Keep loading_active in sync for the active tab so that
                    // BrowserEngine::is_loading() can read it lock-free.
                    guard.sync_loading_active();
                }
                // Trigger a snapshot on navigation complete.
                if matches!(event, LoadEvent::Finished) {
                    request_snapshot(
                        wv,
                        Arc::clone(&frame_lc),
                        Arc::clone(&view_lc),
                        Rc::clone(&in_flight_lc),
                    );
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

        // ── favicon::notify signal (Gap 1) ───────────────────────────────────
        //
        // Fired by WebKit whenever the favicon for the loaded page changes.
        // We synthesise a FaviconUpdate with a `/favicon.ico` URL derived from
        // the page's current origin. The pixel data is not read here — the
        // apps layer fetches it via the favicon cache using the URL.
        //
        // `browser_id = 0` matches the WebKitGTK convention used throughout
        // this backend (no multi-browser-id concept in WebKitGTK Phase B).
        {
            let st = Arc::clone(&engine_state);
            web_view.connect_favicon_notify(move |wv| {
                // Derive `<origin>/favicon.ico` from the current page URI.
                let url = wv.uri().map(|s| s.to_string()).unwrap_or_default();
                let favicon_url = favicon_url_from_page(&url);
                tracing::debug!(
                    tab_id = ?id,
                    favicon_url = %favicon_url,
                    "webkitgtk runtime: favicon notify"
                );
                if let Ok(st) = st.lock()
                    && let Ok(mut updates) = st.favicon_updates.lock()
                {
                    updates.push(FaviconUpdate {
                        browser_id: 0,
                        // FaviconUpdate currently carries pixel data; we push
                        // a zero-size entry to signal the URL. The apps layer
                        // uses the URL from the tab strip, not this entry
                        // directly, so width/height/pixels are 0/empty.
                        width: 0,
                        height: 0,
                        pixels: Vec::new(),
                    });
                    tracing::debug!("webkitgtk runtime: favicon update queued url={favicon_url}");
                }
            });
        }

        // ── mouse-target-changed signal (Gap 2) ──────────────────────────────
        //
        // Fired when the hover target changes. Map the hit-test context to a
        // neutral cursor discriminant `(browser_id, raw_kind)`:
        //   link    → 3  (Cursor::Pointer equivalent)
        //   editable→ 4  (IBeam)
        //   else    → 0  (Arrow / Default)
        //
        // The raw_kind values match the CEF `CursorType` constants used by the
        // apps layer; the mapping is kept minimal for Phase B.
        {
            let st = Arc::clone(&engine_state);
            web_view.connect_mouse_target_changed(move |_wv, hit, _modifiers| {
                let raw_kind: u32 = if hit.context_is_link() {
                    3 // Pointer
                } else if hit.context_is_editable() {
                    4 // IBeam
                } else {
                    0 // Arrow
                };
                if let Ok(st) = st.lock()
                    && let Ok(mut slot) = st.cursor_change.lock()
                {
                    *slot = Some((0, raw_kind));
                }
                tracing::debug!(raw_kind, "webkitgtk runtime: cursor changed");
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
                    zoom: 1.0,
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

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Derive a `/favicon.ico` URL from a page URL.
///
/// Strips the path/query/fragment and appends `favicon.ico`. Falls back to
/// the original URL string on parse failure.
///
/// # Examples
///
/// ```
/// let url = "https://example.com/some/page?q=1";
/// // → "https://example.com/favicon.ico"
/// ```
pub(crate) fn favicon_url_from_page(page_url: &str) -> String {
    // Simple origin extraction: find "://", then take up to the first "/" after
    // the authority.  Avoids pulling in a full URL crate.
    if let Some(authority_start) = page_url.find("://") {
        let after_scheme = &page_url[authority_start + 3..];
        // Authority ends at the first "/" (path), "?" (query), or "#" (fragment).
        let authority_end = after_scheme
            .find(['/', '?', '#'])
            .unwrap_or(after_scheme.len());
        let authority = &after_scheme[..authority_end];
        let scheme = &page_url[..authority_start];
        return format!("{scheme}://{authority}/favicon.ico");
    }
    // Fallback: non-http scheme or malformed URL — return unchanged.
    page_url.to_owned()
}
