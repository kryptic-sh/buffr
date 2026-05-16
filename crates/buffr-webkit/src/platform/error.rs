//! Error types for the WPE WebKit backend.

use thiserror::Error;

/// Errors specific to the WPE WebKit backend.
#[derive(Debug, Error)]
pub enum WebKitError {
    #[error("wpe-webkit initialization failed: {0}")]
    InitFailed(String),

    #[error("wpe-webkit tab not found: {0:?}")]
    TabNotFound(buffr_engine::TabId),

    #[error("wpe-webkit worker timeout")]
    WorkerTimeout,

    #[error("wpe-webkit error: {0}")]
    Other(String),
}

impl From<WebKitError> for buffr_engine::EngineError {
    fn from(e: WebKitError) -> Self {
        match e {
            WebKitError::InitFailed(msg) => buffr_engine::EngineError::InitFailed(msg),
            WebKitError::TabNotFound(id) => {
                buffr_engine::EngineError::Other(format!("tab not found: {id:?}"))
            }
            WebKitError::WorkerTimeout => buffr_engine::EngineError::Other("worker timeout".into()),
            WebKitError::Other(msg) => buffr_engine::EngineError::Other(msg),
        }
    }
}
