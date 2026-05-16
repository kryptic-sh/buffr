//! GLib worker thread stub for the WPE WebKit backend.
//!
//! Phase 1: spawns a parked thread that does nothing. Phase 2 will run a GLib
//! main loop here and own the WebKitWebView instances.

use super::error::WebKitError;

/// Handle to the background worker thread.
///
/// Dropping this handle does not yet join the thread — Phase 2 will add a
/// shutdown channel and a `JoinHandle`.
pub(crate) struct WorkerHandle {
    #[allow(dead_code)]
    thread: std::thread::JoinHandle<()>,
}

/// Spawn the GLib worker thread.
///
/// Phase 1: the thread parks immediately and never wakes. No commands are
/// sent, no WebView is created. The purpose is to prove the thread-spawn path
/// compiles and does not crash on startup.
pub(crate) fn spawn() -> Result<WorkerHandle, WebKitError> {
    tracing::debug!("webkit: spawning worker thread (phase 1 stub — parks immediately)");
    let thread = std::thread::Builder::new()
        .name("buffr-webkit-worker".into())
        .spawn(|| {
            tracing::debug!("webkit: worker thread started (phase 1 stub)");
            loop {
                std::thread::park();
            }
        })
        .map_err(|e| WebKitError::InitFailed(format!("failed to spawn worker thread: {e}")))?;

    Ok(WorkerHandle { thread })
}
