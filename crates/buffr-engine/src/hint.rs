//! Neutral hint-mode types shared across all engine backends.
//!
//! Moved from `buffr-cef::host` in Phase 6b (#95) so the `BrowserEngine`
//! trait can express hint-mode operations without any CEF dependency.

/// Snapshot of the active tab's hint session for statusline rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HintStatus {
    /// Characters the user has typed so far in this session.
    pub typed: String,
    /// Number of visible hint overlays that still match the typed prefix.
    pub match_count: usize,
    /// `true` when the session was started in background-open mode.
    pub background: bool,
}

/// Result returned by hint-session key feeding methods.
///
/// Mirrors the shape of `buffr_modal::engine::Step` but for hint-mode.
/// Moved to `buffr-engine` in Phase 6b (#95) so the `BrowserEngine` trait
/// can reference it without pulling in `buffr-core`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HintAction {
    /// User typed a character that narrowed the candidate set but did not
    /// commit. The UI should call `__buffrHintFilter(typed)` to dim the
    /// non-matching overlays.
    Filter,
    /// One label remained and the typed string equals it. The caller
    /// should dispatch the click and exit hint mode.
    Click(u32),
    /// Background-open variant. The host falls back to a regular click
    /// with a tracing breadcrumb until multi-tab is implemented.
    OpenInBackground(u32),
    /// The user typed a character that no label starts with. The caller
    /// should cancel the hint session.
    Cancel,
}
