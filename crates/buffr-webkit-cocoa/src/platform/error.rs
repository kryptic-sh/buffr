//! Error types for the WebKit Cocoa backend (macOS only).

use thiserror::Error;

/// Errors specific to the WebKit Cocoa backend.
#[derive(Debug, Error)]
pub enum WebKitCocoaError {
    /// Platform is not macOS (compile-time gated, but kept for coherence).
    #[error("webkit-cocoa is not supported on this platform: {0}")]
    Unsupported(String),

    /// Engine or WebView initialisation failed.
    #[error("webkit-cocoa initialization failed: {0}")]
    InitFailed(String),

    /// Tab does not exist.
    #[error("webkit-cocoa tab not found: {0:?}")]
    TabNotFound(buffr_engine::TabId),

    /// Worker channel closed unexpectedly.
    #[error("webkit-cocoa worker channel closed")]
    WorkerGone,

    /// A call to the worker timed out.
    #[error("webkit-cocoa worker timeout")]
    Timeout,
}

impl From<WebKitCocoaError> for buffr_engine::EngineError {
    fn from(e: WebKitCocoaError) -> Self {
        match e {
            WebKitCocoaError::Unsupported(msg) => buffr_engine::EngineError::Other(msg),
            WebKitCocoaError::InitFailed(msg) => buffr_engine::EngineError::InitFailed(msg),
            WebKitCocoaError::TabNotFound(id) => {
                buffr_engine::EngineError::Other(format!("tab not found: {id:?}"))
            }
            WebKitCocoaError::WorkerGone => {
                buffr_engine::EngineError::Other("worker channel closed".into())
            }
            WebKitCocoaError::Timeout => buffr_engine::EngineError::Other("worker timeout".into()),
        }
    }
}
