//! Error types for the blink-cdp backend.

use thiserror::Error;

/// Errors specific to the blink-cdp backend.
#[derive(Debug, Error)]
pub enum BlinkError {
    #[error("chromium binary not found — install chromium, google-chrome, or chromium-browser")]
    ChromiumNotFound,

    #[error("failed to spawn chromium subprocess: {0}")]
    SpawnFailed(#[source] std::io::Error),

    #[error("failed to probe for a free port: {0}")]
    PortProbe(#[source] std::io::Error),

    #[error("CDP probe failed (is port {port} free?): {source}")]
    ProbeFailed {
        port: u16,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("CDP WebSocket connect failed: {0}")]
    WsConnect(String),

    #[error("CDP WebSocket I/O error: {0}")]
    WsIo(String),

    #[error("CDP protocol error: {0}")]
    Protocol(String),

    #[error("CDP command timed out: {method}")]
    Timeout { method: &'static str },

    #[error("CDP worker shut down")]
    WorkerDead,

    #[error("image decode failed: {0}")]
    ImageDecode(String),

    #[error("no active tab")]
    NoActiveTab,

    #[error("tab not found: {0}")]
    TabNotFound(buffr_engine::TabId),
}

impl From<BlinkError> for buffr_engine::EngineError {
    fn from(e: BlinkError) -> Self {
        match e {
            BlinkError::ChromiumNotFound => buffr_engine::EngineError::InitFailed(e.to_string()),
            BlinkError::SpawnFailed(_) => buffr_engine::EngineError::InitFailed(e.to_string()),
            BlinkError::PortProbe(_) => buffr_engine::EngineError::InitFailed(e.to_string()),
            BlinkError::ProbeFailed { .. } => buffr_engine::EngineError::InitFailed(e.to_string()),
            BlinkError::WsConnect(_) => buffr_engine::EngineError::InitFailed(e.to_string()),
            BlinkError::NoActiveTab => buffr_engine::EngineError::NoActiveTab,
            BlinkError::TabNotFound(id) => buffr_engine::EngineError::TabNotFound(id),
            other => buffr_engine::EngineError::Other(other.to_string()),
        }
    }
}
