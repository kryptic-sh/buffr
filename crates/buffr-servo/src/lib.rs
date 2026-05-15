//! Servo backend for buffr-engine.
//!
//! Phase A stub: scaffolding only.
//! Phase B: real Servo integration via the `servo` crate (v0.1.0, crates.io).
//!
//! # Architecture
//!
//! `ServoEngine` drives a worker thread that owns the `!Send + !Sync` Servo
//! and WebView handles.  Commands are sent via `mpsc::SyncSender<Command>`
//! (the `WorkerHandle`).  The OSR pipeline writes BGRA frames into a
//! `SharedOsrFrame` arc that callers clone via `BrowserEngine::osr_frame()`.

pub mod backend;
pub mod engine;
pub mod error;
pub mod input;
pub mod worker;

pub use backend::ServoBackend;
pub use engine::ServoEngine;
pub use error::ServoError;
