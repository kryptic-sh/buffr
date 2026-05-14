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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_event_construction_doesnt_panic() {
        let nav = EngineEvent::Navigation(crate::NavigationEvent {
            tab_id: crate::TabId(1),
            url: "https://example.com".into(),
            state: crate::LoadState::Idle,
        });
        assert!(matches!(nav, EngineEvent::Navigation(_)));

        let paint = EngineEvent::PaintReady { browser_id: 1 };
        assert!(matches!(paint, EngineEvent::PaintReady { browser_id: 1 }));

        let cursor = EngineEvent::CursorChanged(crate::types::CursorChanged {
            browser_id: 2,
            kind: crate::types::CursorKind::Pointer,
        });
        assert!(matches!(cursor, EngineEvent::CursorChanged(_)));

        let audio = EngineEvent::AudioChanged(crate::types::AudioEvent {
            browser_id: 3,
            active: true,
        });
        assert!(matches!(audio, EngineEvent::AudioChanged(_)));

        let popup_created = EngineEvent::PopupCreated {
            browser_id: 4,
            url: "https://popup.example.com".into(),
        };
        assert!(matches!(
            popup_created,
            EngineEvent::PopupCreated { browser_id: 4, .. }
        ));

        let popup_closed = EngineEvent::PopupClosed { browser_id: 5 };
        assert!(matches!(
            popup_closed,
            EngineEvent::PopupClosed { browser_id: 5 }
        ));
    }
}
