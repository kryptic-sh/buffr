//! CEF integration and browser host.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("cef initialization failed")]
    InitFailed,
}

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
