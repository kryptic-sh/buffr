//! Engine error type.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("engine initialization failed: {0}")]
    InitFailed(String),

    #[error("tab not found: {0}")]
    TabNotFound(crate::TabId),

    #[error("no active tab")]
    NoActiveTab,

    #[error("browser creation failed")]
    CreateBrowserFailed,

    #[error("invalid url: {0}")]
    InvalidUrl(String),

    #[error("operation failed: {0}")]
    Other(String),

    /// Returned by backends that have not yet implemented the method.
    ///
    /// `method` carries the trait method name for diagnostics. Phase 4
    /// blink-cdp stubs return this for popup_*, hint_*, find_*, zoom_*,
    /// devtools_*, scheme_handler_*, and other non-core methods.
    #[error("not implemented: {method}")]
    Unimplemented { method: &'static str },
}
