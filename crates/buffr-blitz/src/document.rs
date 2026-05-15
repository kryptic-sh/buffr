//! Per-tab Blitz document wrapper.
//!
//! Each `BlitzTab` owns a real `HtmlDocument` (Stylo CSS + Taffy layout)
//! plus navigation history so `go_back`/`go_forward` work without any
//! browser-side history API.

use blitz_dom::DocumentConfig;
use blitz_html::HtmlDocument;
use blitz_traits::shell::{ColorScheme, Viewport};

use buffr_engine::{TabId, TabSummary};

use crate::error::BlitzError;

/// History stack entry.
#[derive(Clone)]
struct HistEntry {
    url: String,
    /// Cached title used to restore `BlitzTab::title` on back/forward navigation.
    #[allow(dead_code)]
    title: String,
}

/// One tab's complete state.
pub(crate) struct BlitzTab {
    pub id: TabId,
    /// The real Blitz DOM + Stylo + Taffy document.
    pub doc: HtmlDocument,
    /// Full URL of the loaded document (updated after every navigate).
    pub url: String,
    /// Page title extracted from `<title>`.
    pub title: String,
    /// `true` while the initial HTML fetch is in progress.
    pub is_loading: bool,
    /// CSS-zoom multiplier on the paint scale. Default 1.0; clamped to
    /// [0.25, 5.0]. Read by the worker's paint() to combine with the
    /// device pixel ratio at render time.
    pub zoom: f64,
    /// Back-navigation stack (oldest → newest, current NOT included).
    back_stack: Vec<HistEntry>,
    /// Forward-navigation stack (popped on navigate, refilled by go_back).
    fwd_stack: Vec<HistEntry>,
}

impl BlitzTab {
    /// Open a new tab at `url`.  Fetches HTML synchronously via `ureq`;
    /// falls back to an error page on network failure.
    pub(crate) fn open(id: TabId, url: &str, width: u32, height: u32) -> Result<Self, BlitzError> {
        tracing::info!("blitz: open tab {id} → {url}");
        let html = fetch_html(url);
        let doc = make_doc(&html, url, width, height);
        let title = extract_title(&doc);
        Ok(BlitzTab {
            id,
            doc,
            url: url.to_owned(),
            title,
            is_loading: false,
            zoom: 1.0,
            back_stack: Vec::new(),
            fwd_stack: Vec::new(),
        })
    }

    /// Navigate the tab to `url`, pushing the current entry onto the back stack.
    pub(crate) fn navigate(&mut self, url: &str, width: u32, height: u32) {
        tracing::debug!("blitz: tab {} navigate → {url}", self.id);
        let prev = HistEntry {
            url: self.url.clone(),
            title: self.title.clone(),
        };
        self.back_stack.push(prev);
        self.fwd_stack.clear();
        self.load(url, width, height);
    }

    /// Reload the current URL.
    pub(crate) fn reload(&mut self, width: u32, height: u32) {
        tracing::debug!("blitz: tab {} reload {}", self.id, self.url);
        let url = self.url.clone();
        self.load(&url, width, height);
    }

    /// Navigate back `n` steps.  Clamps to available history.
    pub(crate) fn go_back(&mut self, n: usize, width: u32, height: u32) {
        let steps = n.min(self.back_stack.len());
        if steps == 0 {
            tracing::debug!("blitz: tab {} go_back — no history", self.id);
            return;
        }
        // Push current onto fwd stack.
        self.fwd_stack.push(HistEntry {
            url: self.url.clone(),
            title: self.title.clone(),
        });
        // Pop extra intermediate entries into fwd stack.
        for _ in 1..steps {
            if let Some(e) = self.back_stack.pop() {
                self.fwd_stack.push(e);
            }
        }
        if let Some(entry) = self.back_stack.pop() {
            let url = entry.url.clone();
            self.load(&url, width, height);
        }
    }

    /// Navigate forward `n` steps.
    pub(crate) fn go_forward(&mut self, n: usize, width: u32, height: u32) {
        let steps = n.min(self.fwd_stack.len());
        if steps == 0 {
            tracing::debug!("blitz: tab {} go_forward — no forward history", self.id);
            return;
        }
        self.back_stack.push(HistEntry {
            url: self.url.clone(),
            title: self.title.clone(),
        });
        for _ in 1..steps {
            if let Some(e) = self.fwd_stack.pop() {
                self.back_stack.push(e);
            }
        }
        if let Some(entry) = self.fwd_stack.pop() {
            let url = entry.url.clone();
            self.load(&url, width, height);
        }
    }

    /// `true` when there are entries on the back stack.
    pub(crate) fn can_go_back(&self) -> bool {
        !self.back_stack.is_empty()
    }

    /// `true` when there are entries on the forward stack.
    pub(crate) fn can_go_forward(&self) -> bool {
        !self.fwd_stack.is_empty()
    }

    /// Resize the viewport and re-do layout.
    pub(crate) fn resize(&mut self, width: u32, height: u32) {
        tracing::debug!("blitz: tab {} resize {width}×{height}", self.id);
        let vp = Viewport::new(width, height, 1.0, ColorScheme::Light);
        self.doc.set_viewport(vp);
        self.doc.resolve(0.0);
    }

    /// Build a [`TabSummary`] snapshot.
    ///
    /// Not used in Phase B (worker uses `TabRecord::to_summary`); retained
    /// for Phase C direct-access use cases.
    #[allow(dead_code)]
    pub(crate) fn to_summary(&self) -> TabSummary {
        TabSummary {
            id: self.id,
            browser_id: 0,
            title: self.title.clone(),
            url: self.url.clone(),
            progress: 1.0,
            is_loading: self.is_loading,
            pinned: false,
            private: false,
        }
    }

    // ── Internal helpers ─────────────────────────────────────────────────────

    /// Fetch HTML, (re)build the document, run layout.
    fn load(&mut self, url: &str, width: u32, height: u32) {
        self.is_loading = true;
        self.url = url.to_owned();
        let html = fetch_html(url);
        self.doc = make_doc(&html, url, width, height);
        self.title = extract_title(&self.doc);
        self.is_loading = false;
        tracing::debug!("blitz: tab {} loaded '{}'", self.id, self.title);
    }
}

// ── Free helpers ──────────────────────────────────────────────────────────────

/// Fetch raw HTML from `url`.
///
/// Handles `data:` URIs directly, otherwise uses `ureq` for HTTP(S).
/// Returns a simple error-page HTML string on failure.
pub(crate) fn fetch_html(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("data:") {
        // data:[<mediatype>][;base64],<data>
        if let Some(comma_pos) = rest.find(',') {
            let data_part = &rest[comma_pos + 1..];
            let is_base64 = rest[..comma_pos].contains(";base64");
            if is_base64 {
                // Simple base64 decode: just return raw body for now.
                return format!("<html><body>{}</body></html>", data_part);
            } else {
                return percent_decode(data_part);
            }
        }
        return error_page(url, "malformed data: URI");
    }

    match ureq::get(url).call() {
        Ok(mut resp) => match resp.body_mut().read_to_string() {
            Ok(body) => body,
            Err(e) => error_page(url, &e.to_string()),
        },
        Err(e) => {
            tracing::warn!("blitz: fetch {url} failed: {e}");
            error_page(url, &e.to_string())
        }
    }
}

/// Minimal %-decoding for `data:` URI bodies.
fn percent_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(hex) =
                u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16)
        {
            out.push(hex as char);
            i += 3;
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Build and lay out a fresh `HtmlDocument`.
pub(crate) fn make_doc(html: &str, base_url: &str, width: u32, height: u32) -> HtmlDocument {
    let vp = Viewport::new(width, height, 1.0, ColorScheme::Light);
    let config = DocumentConfig {
        viewport: Some(vp),
        base_url: Some(base_url.to_owned()),
        ..Default::default()
    };
    let mut doc = HtmlDocument::from_html(html, config);
    // Run Stylo + Taffy layout immediately.
    doc.resolve(0.0);
    doc
}

/// Extract the document title from a laid-out `HtmlDocument`.
pub(crate) fn extract_title(doc: &HtmlDocument) -> String {
    doc.find_title_node()
        .and_then(|node| {
            // The title node's first child is a text node.
            node.children
                .first()
                .and_then(|child_id| doc.get_node(*child_id))
                .and_then(|text_node| match &text_node.data {
                    blitz_dom::node::NodeData::Text(text_data) => {
                        Some(text_data.content.trim().to_owned())
                    }
                    _ => None,
                })
        })
        .unwrap_or_default()
}

/// Generic error page HTML.
fn error_page(url: &str, reason: &str) -> String {
    format!(
        "<!DOCTYPE html><html><head><title>Error</title></head><body>\
         <h1>Failed to load</h1><p><code>{}</code></p><p>{}</p></body></html>",
        url, reason
    )
}
