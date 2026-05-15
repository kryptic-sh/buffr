//! Error types for the Servo backend.

use thiserror::Error;

/// Errors specific to the Servo backend.
#[derive(Debug, Error)]
pub enum ServoError {
    #[error("servo is not supported on this platform: {0}")]
    Unsupported(String),

    #[error("servo initialization failed: {0}")]
    InitFailed(String),

    #[error("servo rendering context error: {0}")]
    RenderingContext(String),

    #[error("servo tab not found: {0:?}")]
    TabNotFound(buffr_engine::TabId),

    #[error("servo worker thread disconnected")]
    WorkerDisconnected,

    #[error("servo worker command timed out")]
    WorkerTimeout,

    /// URL parse error.
    ///
    /// Note: `url::ParseError` is NOT listed as a `#[from]` dependency here
    /// because the `url` crate is only usable once the `servo` dep conflict is
    /// resolved (it was removed from Cargo.toml as a direct dep when `servo`
    /// was removed).  The variant is kept as a String for the skeleton.
    #[error("servo URL parse error: {0}")]
    UrlParse(String),
}

impl From<ServoError> for buffr_engine::EngineError {
    fn from(e: ServoError) -> Self {
        match e {
            ServoError::Unsupported(msg) => buffr_engine::EngineError::Other(msg),
            ServoError::InitFailed(msg) => buffr_engine::EngineError::InitFailed(msg),
            ServoError::RenderingContext(msg) => buffr_engine::EngineError::Other(msg),
            ServoError::TabNotFound(id) => {
                buffr_engine::EngineError::Other(format!("tab not found: {id:?}"))
            }
            ServoError::WorkerDisconnected => {
                buffr_engine::EngineError::Other("servo worker disconnected".into())
            }
            ServoError::WorkerTimeout => {
                buffr_engine::EngineError::Other("servo worker timed out".into())
            }
            ServoError::UrlParse(msg) => buffr_engine::EngineError::Other(msg),
        }
    }
}
