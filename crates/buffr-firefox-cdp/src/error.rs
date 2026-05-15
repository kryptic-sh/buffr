//! Error types for the Firefox CDP backend.

use thiserror::Error;

/// Errors specific to the Firefox CDP backend.
#[derive(Debug, Error)]
pub enum FirefoxError {
    #[error(
        "firefox binary not found — install firefox, firefox-esr, or firefox-developer-edition"
    )]
    FirefoxNotFound,

    #[error("failed to spawn firefox subprocess: {0}")]
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

    #[error("failed to create Firefox profile directory: {0}")]
    ProfileDirCreate(std::io::Error),

    #[error("tab not found: {0}")]
    TabNotFound(buffr_engine::TabId),
}

impl From<FirefoxError> for buffr_engine::EngineError {
    fn from(e: FirefoxError) -> Self {
        match e {
            FirefoxError::FirefoxNotFound => buffr_engine::EngineError::InitFailed(e.to_string()),
            FirefoxError::SpawnFailed(_) => buffr_engine::EngineError::InitFailed(e.to_string()),
            FirefoxError::ProfileDirCreate(_) => {
                buffr_engine::EngineError::InitFailed(e.to_string())
            }
            FirefoxError::PortProbe(_) => buffr_engine::EngineError::InitFailed(e.to_string()),
            FirefoxError::ProbeFailed { .. } => {
                buffr_engine::EngineError::InitFailed(e.to_string())
            }
            FirefoxError::WsConnect(_) => buffr_engine::EngineError::InitFailed(e.to_string()),
            FirefoxError::NoActiveTab => buffr_engine::EngineError::NoActiveTab,
            FirefoxError::TabNotFound(id) => buffr_engine::EngineError::TabNotFound(id),
            other => buffr_engine::EngineError::Other(other.to_string()),
        }
    }
}
