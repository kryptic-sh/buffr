//! Ladybird backend for buffr-engine.
//!
//! # Phase B
//!
//! This crate implements `Backend` + `BrowserEngine` with a real cxx FFI surface
//! backed by a zero-dependency C++17 stub (`src/shim/ladybird_shim.{h,cpp}`).
//!
//! Key navigation, tab management, OSR resize, and input event forwarding are
//! fully wired.  The C++ stub fills OSR pixels with solid dark-grey (BGRA
//! 0xFF101010).
//!
//! Phase C will replace the stub with real Ladybird WebContent embedding.

pub mod backend;
pub mod engine;
pub mod error;
pub(crate) mod ffi;
pub(crate) mod input;
pub(crate) mod state;

pub use backend::LadybirdBackend;
pub use engine::LadybirdEngine;
pub use error::LadybirdError;
