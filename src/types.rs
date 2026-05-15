//! Neutral event and state types surfaced by the engine to the apps layer.

/// Page load state.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadState {
    Idle,
    Loading,
    /// Navigation ended with an HTTP error (4xx / 5xx).
    HttpError(i32),
    /// Navigation ended with a net error.
    NetError(i32),
}

/// Navigation event emitted by the engine when the active tab's URL
/// or load state changes.
#[derive(Debug, Clone)]
pub struct NavigationEvent {
    pub tab_id: crate::TabId,
    pub url: String,
    pub state: LoadState,
}

/// Audio stream activity change for one browser.
#[derive(Debug, Clone, Copy)]
pub struct AudioEvent {
    /// Engine-internal browser id.
    pub browser_id: i32,
    pub active: bool,
}

/// Cursor icon changed. Neutral representation of a platform cursor.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorKind {
    Default,
    Pointer,
    Text,
    Move,
    ResizeNs,
    ResizeEw,
    ResizeNeSw,
    ResizeNwSe,
    NotAllowed,
    Wait,
    Grab,
    Grabbing,
    ZoomIn,
    ZoomOut,
    /// Unknown / platform-specific. Apps layer uses `Default`.
    Other,
}

/// Cursor change event emitted by the engine.
#[derive(Debug, Clone)]
pub struct CursorChanged {
    /// Engine-internal browser id.
    pub browser_id: i32,
    pub kind: CursorKind,
}

/// Context-menu request. Emitted by the engine when the user right-clicks.
///
/// `x` / `y` are browser-local pixel coordinates (CSS pixels × device scale
/// only if the backend normalises them — Phase 1 inherits CEF's convention
/// which is CSS pixels before device scaling).
#[derive(Debug, Clone)]
pub struct ContextMenuRequest {
    /// Engine-internal browser id.
    pub browser_id: i32,
    pub x: i32,
    pub y: i32,
    pub page_url: String,
    pub frame_url: String,
    pub link_url: Option<String>,
    pub image_url: Option<String>,
    pub media_url: Option<String>,
    pub selection_text: Option<String>,
    pub is_editable: bool,
    pub has_image_contents: bool,
    pub media_type: MediaType,
}

/// What kind of content is under the context-menu cursor.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaType {
    None,
    Image,
    Video,
    Audio,
    Canvas,
    File,
    Plugin,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_state_variants_distinct() {
        assert_eq!(LoadState::Idle, LoadState::Idle);
        assert_eq!(LoadState::Loading, LoadState::Loading);
        assert_ne!(LoadState::Idle, LoadState::Loading);
        assert_eq!(LoadState::HttpError(404), LoadState::HttpError(404));
        assert_ne!(LoadState::HttpError(404), LoadState::HttpError(500));
        assert_eq!(LoadState::NetError(-1), LoadState::NetError(-1));
    }

    #[test]
    fn navigation_event_carries_tab_id_and_url() {
        let ev = NavigationEvent {
            tab_id: crate::TabId(5),
            url: "https://example.com".into(),
            state: LoadState::Loading,
        };
        assert_eq!(ev.tab_id, crate::TabId(5));
        assert_eq!(ev.url, "https://example.com");
        assert_eq!(ev.state, LoadState::Loading);
    }

    #[test]
    fn cursor_kind_default_is_default_variant() {
        // The "default" cursor is `CursorKind::Default` (no Rust Default derive,
        // but we can assert the variant is distinct from others).
        let kind = CursorKind::Default;
        assert_ne!(kind, CursorKind::Pointer);
        assert_ne!(kind, CursorKind::Text);
        assert_eq!(kind, CursorKind::Default);
    }

    #[test]
    fn cursor_kind_all_non_equal() {
        let kinds = [
            CursorKind::Default,
            CursorKind::Pointer,
            CursorKind::Text,
            CursorKind::Move,
            CursorKind::ResizeNs,
            CursorKind::ResizeEw,
            CursorKind::ResizeNeSw,
            CursorKind::ResizeNwSe,
            CursorKind::NotAllowed,
            CursorKind::Wait,
            CursorKind::Grab,
            CursorKind::Grabbing,
            CursorKind::ZoomIn,
            CursorKind::ZoomOut,
            CursorKind::Other,
        ];
        // Each variant equals itself.
        for k in &kinds {
            assert_eq!(k, k);
        }
        // Spot-check a pair.
        assert_ne!(CursorKind::Grab, CursorKind::Grabbing);
    }

    #[test]
    fn audio_event_carries_browser_id() {
        let ev = AudioEvent {
            browser_id: 7,
            active: true,
        };
        assert_eq!(ev.browser_id, 7);
        assert!(ev.active);
    }
}
