//! `buffr-engine` — engine-agnostic browser trait and neutral types.
//!
//! Zero CEF dependency. `buffr-core` and the apps layer program against
//! this interface; `buffr-cef` is the concrete backend.

pub mod backend;
pub mod clipboard;
pub mod engine;
pub mod engine_id;
pub mod error;
pub mod event;
pub mod favicon;
pub mod hint;
pub mod input;
pub mod newtab;
pub mod osr;
pub mod permissions;
pub mod popup;
pub mod profile;
pub mod tab;
pub mod types;

pub use backend::{Backend, BackendOpenOptions, NewTabHtmlProvider};
pub use clipboard::{ClipboardRead, ClipboardReader};
pub use engine::BrowserEngine;
pub use engine_id::EngineId;
pub use error::EngineError;
pub use event::EngineEvent;
pub use favicon::FaviconUpdate;
pub use hint::{HintAction, HintStatus};
pub use input::{KeyEventKind, MouseButton, NeutralKeyEvent};
pub use osr::{OsrFrame, OsrViewState, SharedOsrFrame, SharedOsrViewState};
pub use permissions::{
    PendingPermission, PermissionsQueue, PromptOutcome, drain_queue as drain_permissions_queue,
    new_queue as new_permissions_queue, peek_front as peek_permission_front,
    pop_front as pop_permission_front, queue_len as permissions_queue_len,
};
pub use popup::{
    PendingPopupAlloc, PopupCloseSink, PopupCreateSink, PopupCreated, PopupQueue,
    drain_popup_closes, drain_popup_creates, drain_popup_urls, new_pending_popup_alloc,
    new_popup_close_sink, new_popup_create_sink, new_popup_queue,
};
pub use profile::ProfilePaths;
pub use tab::{TabId, TabOptions, TabSession, TabSummary};
pub use types::{AudioEvent, ContextMenuRequest, CursorChanged, LoadState, NavigationEvent};
