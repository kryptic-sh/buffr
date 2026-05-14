//! Tab identity and summary types.

/// Monotonic tab identifier minted by the engine. Distinct from any
/// engine-internal identifier (e.g. CEF's `Browser::identifier()`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TabId(pub u64);

impl std::fmt::Display for TabId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "tab#{}", self.0)
    }
}

/// Per-tab UI state preserved across tab switches.
#[derive(Debug, Default, Clone)]
pub struct TabSession {
    pub find_query: Option<String>,
    // hint session is opaque at the engine level; carried as a boxed type.
}

/// Options for opening a new tab.
#[derive(Debug, Default, Clone)]
pub struct TabOptions {
    /// Open in the background (don't activate).
    pub background: bool,
    /// Insert at this position in the tab strip (clamped).
    pub insert_idx: Option<usize>,
    /// Mark as pinned immediately.
    pub pinned: bool,
}

/// Copy-friendly snapshot of a tab. Used by chrome / UI threads that
/// don't want to hold the manager mutex.
#[derive(Debug, Clone)]
pub struct TabSummary {
    pub id: TabId,
    /// Engine-internal browser id. Used by the apps layer to correlate
    /// with sinks keyed on this id (favicon downloads, cursor state).
    /// Phase 1 keeps this as `i32` matching CEF's value directly;
    /// a future neutral `BrowserId` newtype can replace it.
    pub browser_id: i32,
    pub title: String,
    pub url: String,
    pub progress: f32,
    pub is_loading: bool,
    pub pinned: bool,
    pub private: bool,
}

/// A popup browser window that has been created and is ready to render.
///
/// Emitted by the engine's lifespan handler when a popup browser
/// (`window.open` / `NEW_POPUP` disposition) comes into existence.
/// The apps layer drains these each tick and spawns a corresponding
/// winit window for each.
pub struct PopupCreated {
    /// Engine-internal browser id for the new popup.
    pub browser_id: i32,
    /// Initial URL (from `on_before_popup`). May be empty.
    pub url: String,
    /// OSR frame buffer shared with the paint handler.
    pub frame: crate::SharedOsrFrame,
    /// OSR viewport state. The apps layer writes width/height on resize.
    pub view: crate::SharedOsrViewState,
}
