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
}
