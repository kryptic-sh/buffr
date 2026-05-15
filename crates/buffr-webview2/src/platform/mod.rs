//! WebView2 platform implementation — compiled on Windows only.

pub mod backend;
pub mod engine;
pub mod error;
pub(crate) mod input;
pub(crate) mod osr;
pub(crate) mod runtime;
pub(crate) mod worker;

pub use backend::WebView2Backend;
pub use engine::WebView2Engine;
pub use error::WebView2Error;
