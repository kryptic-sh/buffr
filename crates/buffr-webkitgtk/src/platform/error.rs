//! Error types for the WebKitGTK backend.

use thiserror::Error;

/// Errors specific to the WebKitGTK backend.
#[derive(Debug, Error)]
pub enum WebKitGtkError {
    #[error("webkitgtk is not supported on this platform: {0}")]
    Unsupported(String),

    #[error("webkitgtk initialization failed: {0}")]
    InitFailed(String),

    #[error("webkitgtk tab not found: {0:?}")]
    TabNotFound(buffr_engine::TabId),

    #[error("webkitgtk worker timeout")]
    WorkerTimeout,
}

impl From<WebKitGtkError> for buffr_engine::EngineError {
    fn from(e: WebKitGtkError) -> Self {
        match e {
            WebKitGtkError::Unsupported(msg) => buffr_engine::EngineError::Other(msg),
            WebKitGtkError::InitFailed(msg) => buffr_engine::EngineError::InitFailed(msg),
            WebKitGtkError::TabNotFound(id) => {
                buffr_engine::EngineError::Other(format!("tab not found: {id:?}"))
            }
            WebKitGtkError::WorkerTimeout => {
                buffr_engine::EngineError::Other("worker timeout".into())
            }
        }
    }
}
