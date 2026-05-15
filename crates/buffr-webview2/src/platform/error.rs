//! Error types for the WebView2 backend.

use thiserror::Error;

/// Errors specific to the WebView2 backend.
#[derive(Debug, Error)]
pub enum WebView2Error {
    #[error("webview2 is not supported on this platform: {0}")]
    Unsupported(String),

    #[error("webview2 initialization failed: {0}")]
    InitFailed(String),

    #[error("webview2 tab not found: {0:?}")]
    TabNotFound(buffr_engine::TabId),

    #[error("webview2 worker timeout")]
    WorkerTimeout,

    #[error("webview2 COM error: HRESULT {0:#010x}")]
    ComError(u32),
}

impl From<WebView2Error> for buffr_engine::EngineError {
    fn from(e: WebView2Error) -> Self {
        match e {
            WebView2Error::Unsupported(msg) => buffr_engine::EngineError::Other(msg),
            WebView2Error::InitFailed(msg) => buffr_engine::EngineError::InitFailed(msg),
            WebView2Error::TabNotFound(id) => {
                buffr_engine::EngineError::Other(format!("tab not found: {id:?}"))
            }
            WebView2Error::WorkerTimeout => {
                buffr_engine::EngineError::Other("worker timeout".into())
            }
            WebView2Error::ComError(hr) => {
                buffr_engine::EngineError::Other(format!("COM error: HRESULT {hr:#010x}"))
            }
        }
    }
}
