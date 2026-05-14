//! Neutral event and state types surfaced by the engine to the apps layer.

/// Page load state.
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
