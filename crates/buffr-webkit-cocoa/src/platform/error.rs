//! Error types for the WebKit Cocoa backend.

use thiserror::Error;

/// Errors specific to the WebKit Cocoa backend.
#[derive(Debug, Error)]
pub enum WebKitCocoaError {
    #[error("webkit-cocoa is not supported on this platform: {0}")]
    Unsupported(String),

    #[error("webkit-cocoa initialization failed: {0}")]
    InitFailed(String),
}

impl From<WebKitCocoaError> for buffr_engine::EngineError {
    fn from(e: WebKitCocoaError) -> Self {
        match e {
            WebKitCocoaError::Unsupported(msg) => buffr_engine::EngineError::Other(msg),
            WebKitCocoaError::InitFailed(msg) => buffr_engine::EngineError::InitFailed(msg),
        }
    }
}
