//! `buffr-engine` — engine-agnostic browser trait and neutral types.
//!
//! Zero CEF dependency. `buffr-core` and the apps layer program against
//! this interface; `buffr-cef` is the concrete backend.

pub mod clipboard;
pub mod engine;
pub mod engine_id;
pub mod error;
pub mod event;
pub mod favicon;
pub mod hint;
pub mod input;
pub mod osr;
pub mod popup;
pub mod tab;
pub mod types;

pub use clipboard::{ClipboardRead, ClipboardReader};
pub use engine::BrowserEngine;
pub use engine_id::EngineId;
pub use error::EngineError;
pub use event::EngineEvent;
pub use favicon::FaviconUpdate;
pub use hint::{HintAction, HintStatus};
pub use input::{KeyEventKind, MouseButton, NeutralKeyEvent};
pub use osr::{OsrFrame, OsrViewState, SharedOsrFrame, SharedOsrViewState};
pub use popup::{
    PendingPopupAlloc, PopupCloseSink, PopupCreateSink, PopupCreated, PopupQueue,
    drain_popup_closes, drain_popup_creates, drain_popup_urls, new_pending_popup_alloc,
    new_popup_close_sink, new_popup_create_sink, new_popup_queue,
};
pub use tab::{TabId, TabOptions, TabSession, TabSummary};
pub use types::{AudioEvent, ContextMenuRequest, CursorChanged, LoadState, NavigationEvent};
