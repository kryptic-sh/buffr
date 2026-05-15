//! Error types for the WebKitGTK backend.

use thiserror::Error;

/// Errors specific to the WebKitGTK backend.
#[derive(Debug, Error)]
pub enum WebKitGtkError {
    #[error("webkitgtk is not supported on this platform: {0}")]
    Unsupported(String),

    #[error("webkitgtk initialization failed: {0}")]
    InitFailed(String),
}

impl From<WebKitGtkError> for buffr_engine::EngineError {
    fn from(e: WebKitGtkError) -> Self {
        match e {
            WebKitGtkError::Unsupported(msg) => buffr_engine::EngineError::Other(msg),
            WebKitGtkError::InitFailed(msg) => buffr_engine::EngineError::InitFailed(msg),
        }
    }
}
