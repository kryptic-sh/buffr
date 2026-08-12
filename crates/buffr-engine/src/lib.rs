//! Engine-agnostic browser trait + neutral types for buffr.
//!
//! `buffr-engine` defines the [`BrowserEngine`] trait and all shared types
//! that backends and the apps layer exchange.  It has **zero** dependency on
//! any browser runtime — no CEF, no CDP, no system libraries.  Any code that
//! needs to be portable across backends should depend only on this crate.
//!
//! # Position in the workspace
//!
//! ```text
//! apps/buffr  ──depends──►  buffr-engine  (trait + types, this crate)
//!                                │
//!              ┌─────────────────┴──────────────────┐
//!              ▼                                     ▼
//!         buffr-cef                             buffr-webkit
//!    (CEF concrete backend)               (WPE WebKit native backend)
//! ```
//!
//! The apps layer and `buffr-core` import `buffr-engine` exclusively; they
//! never reach into a backend crate directly.  Backend crates implement
//! [`BrowserEngine`] and expose a single constructor to the application.
//!
//! # Key types
//!
//! | Type | Role |
//! |---|---|
//! | [`BrowserEngine`] | Central trait — one impl per backend |
//! | [`EngineId`] | Unique handle identifying a live engine instance |
//! | [`OsrFrame`] / [`SharedOsrFrame`] | Off-screen render buffer + sharing wrapper |
//! | [`TabId`] / [`TabSummary`] | Lightweight tab identity and metadata |
//! | [`PermissionsQueue`] | Lock-free queue for pending browser permission prompts |
//! | [`PopupQueue`] | Lock-free queue for pending popup window requests |
//!
//! # Phases
//!
//! The v0.9.0 capability surface targeted for both backends covers:
//! - Permissions: camera, microphone, geolocation, notifications, MIDI
//! - Downloads: start, progress, cancel, open
//! - Find-in-page: start, next/prev, close, result count
//! - Context menu: build model, handle selection, image/media buckets
//! - IME: composition start/update/end, candidate window hints
//! - Custom schemes: `buffr-src:` handler registration
//! - Picture-in-Picture

pub mod backend;
pub mod clipboard;
pub mod engine;
pub mod engine_id;
pub mod error;
pub mod favicon;
pub mod hint;
pub mod input;
pub mod internal_server;
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
pub use favicon::FaviconUpdate;
pub use hint::{HintAction, HintStatus};
pub use input::{KeyEventKind, MouseButton, NeutralKeyEvent};
pub use osr::{OsrFrame, OsrViewState, SharedOsrFrame, SharedOsrViewState};
pub use permissions::{
    PendingPermission, PermissionsQueue, PromptOutcome, drain_queue as drain_permissions_queue,
};
pub use popup::{
    PopupCloseSink, PopupCreateSink, PopupCreated, PopupQueue, drain_popup_closes,
    drain_popup_creates, drain_popup_targets, new_popup_close_sink, new_popup_create_sink,
    new_popup_queue,
};
pub use profile::ProfilePaths;
pub use tab::{TabId, TabSummary};
pub use types::{AudioEvent, ContextMenuRequest, WaylandNativeHandles};

/// Re-export of the raw-window-handle crate so backends + apps share the
/// same RawWindowHandle / RawDisplayHandle types without independent
/// version-pinning.
pub use raw_window_handle;
