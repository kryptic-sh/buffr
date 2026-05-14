//! `buffr-engine` — engine-agnostic browser trait and neutral types.
//!
//! Zero CEF dependency. `buffr-core` and the apps layer program against
//! this interface; `buffr-cef` is the concrete backend.

pub mod error;
pub mod event;
pub mod input;
pub mod osr;
pub mod tab;
pub mod types;

pub use error::EngineError;
pub use event::EngineEvent;
pub use input::{KeyEventKind, MouseButton, NeutralKeyEvent};
pub use osr::{OsrFrame, OsrViewState, SharedOsrFrame, SharedOsrViewState};
pub use tab::{PopupCreated, TabId, TabOptions, TabSession, TabSummary};
pub use types::{AudioEvent, ContextMenuRequest, CursorChanged, LoadState, NavigationEvent};
