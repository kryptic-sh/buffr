//! Context-menu hit-test wiring for the blink-cdp backend (Phase 8c, #87).
//!
//! # Approach
//!
//! A small JS shim is injected via `Page.addScriptToEvaluateOnNewDocument`
//! that listens for `contextmenu` events in capture phase (so
//! `e.preventDefault()` suppresses Chromium's native menu), probes the DOM
//! element under the cursor, and posts a JSON payload via the CDP binding
//! `__buffrContextMenu`.
//!
//! The worker thread receives `Runtime.bindingCalled` events for the binding
//! name and parses the payload into a neutral
//! [`buffr_engine::ContextMenuRequest`], which is pushed onto a
//! `Arc<Mutex<VecDeque<ContextMenuRequest>>>` sink.
//!
//! The engine drains the sink in `drain_context_menu_requests`.
//!
//! # Payload shape (JSON)
//!
//! ```json
//! {
//!   "x": 320,
//!   "y": 240,
//!   "pageUrl": "https://example.com/",
//!   "frameUrl": "https://example.com/",
//!   "linkUrl": "https://example.com/link",   // optional
//!   "srcUrl":  "https://example.com/img.png", // optional
//!   "mediaType": "image",                     // "none"|"image"|"video"|"audio"
//!   "selectionText": "selected text",         // optional
//!   "isEditable": false
//! }
//! ```
//!
//! # Deferred
//!
//! - iframe nesting: `contextmenu` capture on the top window does not
//!   fire when the cursor is inside a cross-origin iframe. Follow-up: inject
//!   the shim in every frame via `Runtime.onExecutionContextCreated` +
//!   `Runtime.evaluate` per context.
//! - Rich edit menus (copy/paste/spell-check): `isEditable: true` flag is
//!   sufficient for Phase 8c; inline edit context is future work.
//! - `data:` URL image saves: apps layer should handle; deferred.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use buffr_engine::ContextMenuRequest;
use buffr_engine::types::MediaType;

// ── Sink type ─────────────────────────────────────────────────────────────────

/// Shared queue into which the worker pushes context-menu requests and from
/// which the engine drains them.
pub type ContextMenuSink = Arc<Mutex<VecDeque<ContextMenuRequest>>>;

/// Construct a new, empty [`ContextMenuSink`].
pub fn new_context_menu_sink() -> ContextMenuSink {
    Arc::new(Mutex::new(VecDeque::new()))
}

// ── JS shim ───────────────────────────────────────────────────────────────────

/// Generate the JS shim injected via `Page.addScriptToEvaluateOnNewDocument`.
///
/// The shim intercepts `contextmenu` events in capture phase, walks the DOM
/// ancestor chain to collect link/media metadata, and posts the result via
/// `window.__buffrContextMenu(payload)`.
///
/// `e.preventDefault()` is called so Chromium's native context menu does not
/// appear on top of buffr's overlay.
pub fn context_menu_shim_js() -> String {
    r#"
(function () {
  'use strict';

  document.addEventListener('contextmenu', function (e) {
    e.preventDefault();

    var linkUrl = null;
    var srcUrl = null;
    var mediaType = 'none';
    var isEditable = false;

    // Walk ancestor chain from target upward.
    var el = e.target;
    while (el && el !== document) {
      var tag = el.tagName ? el.tagName.toUpperCase() : '';

      // Link detection.
      if (tag === 'A' && el.href && !linkUrl) {
        linkUrl = el.href;
      }

      // Image detection.
      if (tag === 'IMG' && el.src && !srcUrl) {
        srcUrl = el.src;
        mediaType = 'image';
      }

      // Video detection.
      if (tag === 'VIDEO' && !srcUrl) {
        srcUrl = el.currentSrc || el.src || null;
        mediaType = 'video';
      }

      // Audio detection.
      if (tag === 'AUDIO' && !srcUrl) {
        srcUrl = el.currentSrc || el.src || null;
        mediaType = 'audio';
      }

      // Editable detection.
      if (!isEditable) {
        if (tag === 'INPUT' || tag === 'TEXTAREA') {
          isEditable = true;
        } else if (el.isContentEditable) {
          isEditable = true;
        }
      }

      el = el.parentNode;
    }

    // Selection text.
    var selectionText = null;
    try {
      var sel = window.getSelection();
      if (sel && sel.toString().length > 0) {
        selectionText = sel.toString();
      }
    } catch (_) {}

    var payload = JSON.stringify({
      x: Math.round(e.clientX),
      y: Math.round(e.clientY),
      pageUrl: window.location.href || '',
      frameUrl: window.location.href || '',
      linkUrl: linkUrl,
      srcUrl: srcUrl,
      mediaType: mediaType,
      selectionText: selectionText,
      isEditable: isEditable
    });

    try {
      window.__buffrContextMenu(payload);
    } catch (_) {}
  }, true /* capture */);
})();
"#
    .to_string()
}

// ── Payload parser ────────────────────────────────────────────────────────────

/// Parse a `__buffrContextMenu` binding payload (JSON string) into a
/// [`ContextMenuRequest`].
///
/// `browser_id` is always `0` for the blink-cdp backend (no numeric browser
/// id exists over CDP).
///
/// Returns `None` if the payload is invalid JSON or missing required fields.
pub fn parse_context_menu_binding(payload: &str) -> Option<ContextMenuRequest> {
    let v: serde_json::Value = serde_json::from_str(payload).ok()?;

    let x = v.get("x").and_then(|x| x.as_i64()).unwrap_or(0) as i32;
    let y = v.get("y").and_then(|y| y.as_i64()).unwrap_or(0) as i32;

    let page_url = v
        .get("pageUrl")
        .and_then(|u| u.as_str())
        .unwrap_or("")
        .to_owned();
    let frame_url = v
        .get("frameUrl")
        .and_then(|u| u.as_str())
        .unwrap_or("")
        .to_owned();

    let link_url = v
        .get("linkUrl")
        .and_then(|u| u.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    let src_url = v
        .get("srcUrl")
        .and_then(|u| u.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    let media_type_str = v
        .get("mediaType")
        .and_then(|m| m.as_str())
        .unwrap_or("none");
    let media_type = match media_type_str {
        "image" => MediaType::Image,
        "video" => MediaType::Video,
        "audio" => MediaType::Audio,
        _ => MediaType::None,
    };

    let selection_text = v
        .get("selectionText")
        .and_then(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    let is_editable = v
        .get("isEditable")
        .and_then(|b| b.as_bool())
        .unwrap_or(false);

    // image_url and media_url: split by media type to match the neutral shape.
    let image_url = if media_type == MediaType::Image {
        src_url.clone()
    } else {
        None
    };
    let media_url = if matches!(media_type, MediaType::Video | MediaType::Audio) {
        src_url
    } else {
        None
    };

    Some(ContextMenuRequest {
        browser_id: 0,
        x,
        y,
        page_url,
        frame_url,
        link_url,
        image_url,
        media_url,
        selection_text,
        is_editable,
        has_image_contents: media_type == MediaType::Image,
        media_type,
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── shim JS tests ─────────────────────────────────────────────────────────

    #[test]
    fn shim_js_contains_binding_name() {
        let js = context_menu_shim_js();
        assert!(
            js.contains("__buffrContextMenu"),
            "shim must reference the CDP binding name"
        );
    }

    #[test]
    fn shim_js_prevents_default() {
        let js = context_menu_shim_js();
        assert!(
            js.contains("e.preventDefault()"),
            "shim must call preventDefault to suppress the native menu"
        );
    }

    #[test]
    fn shim_js_uses_capture_phase() {
        let js = context_menu_shim_js();
        // capture-phase addEventListener passes `true` as the third argument.
        assert!(
            js.contains("true /* capture */") || js.contains(", true"),
            "shim must listen in capture phase"
        );
    }

    #[test]
    fn shim_js_detects_link() {
        let js = context_menu_shim_js();
        assert!(js.contains("linkUrl"), "shim must extract link URLs");
        assert!(js.contains("el.href"), "shim must read href");
    }

    #[test]
    fn shim_js_detects_image() {
        let js = context_menu_shim_js();
        assert!(js.contains("IMG"), "shim must detect IMG elements");
        assert!(js.contains("mediaType"), "shim must set mediaType");
    }

    #[test]
    fn shim_js_detects_video_audio() {
        let js = context_menu_shim_js();
        assert!(js.contains("VIDEO"), "shim must detect VIDEO elements");
        assert!(js.contains("AUDIO"), "shim must detect AUDIO elements");
    }

    #[test]
    fn shim_js_detects_editable() {
        let js = context_menu_shim_js();
        assert!(js.contains("isEditable"), "shim must set isEditable");
        assert!(
            js.contains("isContentEditable"),
            "shim must check contenteditable"
        );
    }

    #[test]
    fn shim_js_captures_selection() {
        let js = context_menu_shim_js();
        assert!(
            js.contains("getSelection"),
            "shim must capture selection text"
        );
    }

    #[test]
    fn shim_js_not_empty() {
        let js = context_menu_shim_js();
        assert!(!js.trim().is_empty(), "shim JS must not be empty");
    }

    // ── parse_context_menu_binding tests ──────────────────────────────────────

    #[test]
    fn parse_plain_page_click() {
        let payload = r#"{
            "x": 100, "y": 200,
            "pageUrl": "https://example.com/",
            "frameUrl": "https://example.com/",
            "linkUrl": null,
            "srcUrl": null,
            "mediaType": "none",
            "selectionText": null,
            "isEditable": false
        }"#;
        let req = parse_context_menu_binding(payload).expect("should parse");
        assert_eq!(req.x, 100);
        assert_eq!(req.y, 200);
        assert_eq!(req.page_url, "https://example.com/");
        assert_eq!(req.media_type, MediaType::None);
        assert!(req.link_url.is_none());
        assert!(req.image_url.is_none());
        assert!(req.media_url.is_none());
        assert!(req.selection_text.is_none());
        assert!(!req.is_editable);
        assert!(!req.has_image_contents);
        assert_eq!(req.browser_id, 0);
    }

    #[test]
    fn parse_link_click() {
        let payload = r#"{
            "x": 50, "y": 60,
            "pageUrl": "https://example.com/",
            "frameUrl": "https://example.com/",
            "linkUrl": "https://example.com/page",
            "srcUrl": null,
            "mediaType": "none",
            "selectionText": null,
            "isEditable": false
        }"#;
        let req = parse_context_menu_binding(payload).expect("should parse");
        assert_eq!(req.link_url.as_deref(), Some("https://example.com/page"));
        assert!(req.image_url.is_none());
        assert_eq!(req.media_type, MediaType::None);
    }

    #[test]
    fn parse_image_click() {
        let payload = r#"{
            "x": 10, "y": 20,
            "pageUrl": "https://example.com/",
            "frameUrl": "https://example.com/",
            "linkUrl": null,
            "srcUrl": "https://example.com/photo.jpg",
            "mediaType": "image",
            "selectionText": null,
            "isEditable": false
        }"#;
        let req = parse_context_menu_binding(payload).expect("should parse");
        assert_eq!(req.media_type, MediaType::Image);
        assert_eq!(
            req.image_url.as_deref(),
            Some("https://example.com/photo.jpg")
        );
        assert!(req.media_url.is_none());
        assert!(req.has_image_contents);
    }

    #[test]
    fn parse_video_click() {
        let payload = r#"{
            "x": 30, "y": 40,
            "pageUrl": "https://example.com/",
            "frameUrl": "https://example.com/",
            "linkUrl": null,
            "srcUrl": "https://example.com/clip.mp4",
            "mediaType": "video",
            "selectionText": null,
            "isEditable": false
        }"#;
        let req = parse_context_menu_binding(payload).expect("should parse");
        assert_eq!(req.media_type, MediaType::Video);
        assert_eq!(
            req.media_url.as_deref(),
            Some("https://example.com/clip.mp4")
        );
        assert!(req.image_url.is_none());
        assert!(!req.has_image_contents);
    }

    #[test]
    fn parse_audio_click() {
        let payload = r#"{
            "x": 5, "y": 6,
            "pageUrl": "https://example.com/",
            "frameUrl": "https://example.com/",
            "linkUrl": null,
            "srcUrl": "https://example.com/track.ogg",
            "mediaType": "audio",
            "selectionText": null,
            "isEditable": false
        }"#;
        let req = parse_context_menu_binding(payload).expect("should parse");
        assert_eq!(req.media_type, MediaType::Audio);
        assert_eq!(
            req.media_url.as_deref(),
            Some("https://example.com/track.ogg")
        );
        assert!(req.image_url.is_none());
    }

    #[test]
    fn parse_selection_text() {
        let payload = r#"{
            "x": 0, "y": 0,
            "pageUrl": "https://example.com/",
            "frameUrl": "https://example.com/",
            "linkUrl": null,
            "srcUrl": null,
            "mediaType": "none",
            "selectionText": "hello world",
            "isEditable": false
        }"#;
        let req = parse_context_menu_binding(payload).expect("should parse");
        assert_eq!(req.selection_text.as_deref(), Some("hello world"));
    }

    #[test]
    fn parse_editable_field() {
        let payload = r#"{
            "x": 0, "y": 0,
            "pageUrl": "https://example.com/",
            "frameUrl": "https://example.com/",
            "linkUrl": null,
            "srcUrl": null,
            "mediaType": "none",
            "selectionText": null,
            "isEditable": true
        }"#;
        let req = parse_context_menu_binding(payload).expect("should parse");
        assert!(req.is_editable);
    }

    #[test]
    fn parse_invalid_json_returns_none() {
        assert!(parse_context_menu_binding("not json at all").is_none());
        assert!(parse_context_menu_binding("").is_none());
    }

    #[test]
    fn parse_missing_optional_fields_uses_defaults() {
        // Minimal payload — only required structural fields.
        let payload =
            r#"{"x": 1, "y": 2, "pageUrl": "https://a.com/", "frameUrl": "https://a.com/"}"#;
        let req = parse_context_menu_binding(payload).expect("should parse minimal payload");
        assert_eq!(req.x, 1);
        assert_eq!(req.y, 2);
        assert_eq!(req.media_type, MediaType::None);
        assert!(!req.is_editable);
        assert!(!req.has_image_contents);
    }

    #[test]
    fn parse_image_sets_has_image_contents() {
        let payload = r#"{
            "x": 0, "y": 0,
            "pageUrl": "", "frameUrl": "",
            "linkUrl": null,
            "srcUrl": "https://example.com/a.png",
            "mediaType": "image",
            "selectionText": null,
            "isEditable": false
        }"#;
        let req = parse_context_menu_binding(payload).unwrap();
        assert!(req.has_image_contents);
        assert_eq!(req.media_type, MediaType::Image);
    }

    #[test]
    fn parse_non_image_does_not_set_has_image_contents() {
        let payload = r#"{
            "x": 0, "y": 0,
            "pageUrl": "", "frameUrl": "",
            "linkUrl": null,
            "srcUrl": "https://example.com/v.mp4",
            "mediaType": "video",
            "selectionText": null,
            "isEditable": false
        }"#;
        let req = parse_context_menu_binding(payload).unwrap();
        assert!(!req.has_image_contents);
    }

    #[test]
    fn parse_link_and_image_together() {
        // An <a> wrapping an <img> — linkUrl + image srcUrl both present.
        let payload = r#"{
            "x": 10, "y": 10,
            "pageUrl": "https://example.com/",
            "frameUrl": "https://example.com/",
            "linkUrl": "https://example.com/dest",
            "srcUrl":  "https://example.com/thumb.jpg",
            "mediaType": "image",
            "selectionText": null,
            "isEditable": false
        }"#;
        let req = parse_context_menu_binding(payload).unwrap();
        assert_eq!(req.link_url.as_deref(), Some("https://example.com/dest"));
        assert_eq!(
            req.image_url.as_deref(),
            Some("https://example.com/thumb.jpg")
        );
        assert_eq!(req.media_type, MediaType::Image);
    }
}
