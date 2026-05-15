//! Error types for the WebView2 backend.

use thiserror::Error;

/// Errors specific to the WebView2 backend.
#[derive(Debug, Error)]
pub enum WebView2Error {
    #[error("webview2 is not supported on this platform: {0}")]
    Unsupported(String),

    #[error("webview2 initialization failed: {0}")]
    InitFailed(String),
}

impl From<WebView2Error> for buffr_engine::EngineError {
    fn from(e: WebView2Error) -> Self {
        match e {
            WebView2Error::Unsupported(msg) => buffr_engine::EngineError::Other(msg),
            WebView2Error::InitFailed(msg) => buffr_engine::EngineError::InitFailed(msg),
        }
    }
}
