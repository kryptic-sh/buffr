//! Servo backend for buffr-engine.
//!
//! Real Servo integration via the `servo` crate. `ServoEngine` implements
//! `Backend` + `BrowserEngine`; navigation, tab management, OSR readback,
//! and input forwarding are wired through `worker.rs`. Built standalone
//! (umbrella `workspace.exclude`) because Servo's stylo pin diverges from
//! the Blitz-driven stylo elsewhere in the workspace.
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
