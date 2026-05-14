//! Engine-level event type emitted from the engine to the apps layer.
//!
//! Phase 1 uses channel-free sinks (shared `Arc<Mutex<VecDeque>>`) that
//! match the existing buffr-core architecture. `EngineEvent` is kept here
//! as a placeholder so future phases can switch to an mpsc channel without
//! a breaking change.

/// Events the engine emits to its consumer.
///
/// Phase 1 note: these are not yet used via a channel — the existing sink
/// types (address_sink, find_sink, etc.) in `buffr-cef` carry the data
/// directly. This enum exists for documentation + future migration.
#[derive(Debug, Clone)]
pub enum EngineEvent {
    /// A tab's URL or load state changed.
    Navigation(crate::NavigationEvent),
    /// A paint frame is ready (main or popup).
    PaintReady { browser_id: i32 },
    /// Cursor changed.
    CursorChanged(crate::CursorChanged),
    /// Audio activity changed.
    AudioChanged(crate::AudioEvent),
    /// A popup window was created.
    PopupCreated { browser_id: i32, url: String },
    /// A popup window was closed.
    PopupClosed { browser_id: i32 },
}
