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

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn tab_id_display() {
        assert_eq!(format!("{}", TabId(42)), "tab#42");
        assert_eq!(format!("{}", TabId(0)), "tab#0");
    }

    #[test]
    fn tab_id_ordering() {
        assert!(TabId(1) < TabId(2));
        assert!(TabId(100) > TabId(99));
        assert_eq!(TabId(5), TabId(5));
    }

    #[test]
    fn tab_id_hash_eq() {
        let mut set = HashSet::new();
        set.insert(TabId(10));
        assert!(set.contains(&TabId(10)));
        assert!(!set.contains(&TabId(11)));
    }

    #[test]
    fn tab_id_clone_copy() {
        let a = TabId(7);
        let b = a; // Copy
        #[allow(clippy::clone_on_copy)]
        let c = a.clone(); // exercise Clone impl explicitly
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn tab_summary_fields_accessible() {
        let summary = TabSummary {
            id: TabId(1),
            browser_id: 42,
            title: "Test".to_string(),
            url: "https://example.com".to_string(),
            progress: 0.5,
            is_loading: true,
            pinned: false,
            private: false,
        };
        assert_eq!(summary.id, TabId(1));
        assert_eq!(summary.browser_id, 42);
        assert_eq!(summary.title, "Test");
        assert_eq!(summary.url, "https://example.com");
        assert!((summary.progress - 0.5).abs() < f32::EPSILON);
        assert!(summary.is_loading);
        assert!(!summary.pinned);
        assert!(!summary.private);
    }
}
