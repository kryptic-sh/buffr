//! Windows idle-inhibit backend using `SetThreadExecutionState`.
//!
//! ## API
//!
//! `SetThreadExecutionState` from `kernel32` (`Win32_System_Power`) prevents
//! display sleep. Calling it with `ES_CONTINUOUS | ES_DISPLAY_REQUIRED` makes
//! the request sticky across thread idle until explicitly cleared. Calling it
//! with `ES_CONTINUOUS` alone clears the display-required flag.
//!
//! The function is always available on Windows — no optional feature or runtime
//! probe is needed.
//!
//! ## Thread affinity
//!
//! `SetThreadExecutionState` tracks state **per calling thread**. Acquiring on
//! one thread and releasing from another would not actually clear the flag. To
//! guarantee that both calls happen on the same OS thread, all
//! `SetThreadExecutionState` invocations are made from a dedicated worker
//! thread. Commands arrive via `mpsc::sync_channel(4)`.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender},
    },
    thread,
    time::Duration,
};

use windows_sys::Win32::System::Power::{
    ES_CONTINUOUS, ES_DISPLAY_REQUIRED, EXECUTION_STATE, SetThreadExecutionState,
};

use super::{IdleInhibitor, InhibitError};

// ── Command channel ───────────────────────────────────────────────────────────

enum InhibitCmd {
    Acquire,
    Release,
    Shutdown,
}

// ── Public inhibitor struct ───────────────────────────────────────────────────

/// Windows idle inhibitor using `SetThreadExecutionState`.
pub(super) struct WindowsInhibitor {
    tx: SyncSender<InhibitCmd>,
    active: Arc<AtomicBool>,
    /// Join handle so Drop can wait briefly for the worker.
    worker: Option<thread::JoinHandle<()>>,
}

impl std::fmt::Debug for WindowsInhibitor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowsInhibitor")
            .field("active", &self.active.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl IdleInhibitor for WindowsInhibitor {
    fn acquire(&self) -> Result<(), InhibitError> {
        if self.active.load(Ordering::Relaxed) {
            return Ok(()); // idempotent
        }
        self.tx
            .send(InhibitCmd::Acquire)
            .map_err(|_| InhibitError::PlatformError("windows worker thread disconnected".into()))
    }

    fn release(&self) -> Result<(), InhibitError> {
        if !self.active.load(Ordering::Relaxed) {
            return Ok(()); // idempotent
        }
        self.tx
            .send(InhibitCmd::Release)
            .map_err(|_| InhibitError::PlatformError("windows worker thread disconnected".into()))
    }

    fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }
}

impl Drop for WindowsInhibitor {
    fn drop(&mut self) {
        // Best-effort: ask the worker to release and exit. Ignore send errors
        // (the worker may have already exited on its own).
        let _ = self.tx.send(InhibitCmd::Release);
        let _ = self.tx.send(InhibitCmd::Shutdown);

        // Wait for the worker to exit, but give it at most 100 ms so we
        // don't stall application shutdown. Returns as soon as the
        // worker finishes — only sleeps the full window if it's stuck.
        if let Some(handle) = self.worker.take() {
            let (done_tx, done_rx) = mpsc::sync_channel::<()>(1);
            thread::spawn(move || {
                let _ = handle.join();
                let _ = done_tx.send(());
            });
            let _ = done_rx.recv_timeout(Duration::from_millis(100));
            // On timeout the watcher detaches naturally; process exit reaps it.
        }
    }
}

// ── Constructor ───────────────────────────────────────────────────────────────

/// Construct a [`WindowsInhibitor`].
///
/// The `window` parameter is accepted for API symmetry with other backends
/// but is not used — `SetThreadExecutionState` is per-thread/system, not
/// per-window.
pub(super) fn new(
    _window: std::sync::Arc<winit::window::Window>,
) -> Result<Box<dyn IdleInhibitor>, InhibitError> {
    let (tx, rx) = mpsc::sync_channel::<InhibitCmd>(4);
    let active = Arc::new(AtomicBool::new(false));
    let active_worker = active.clone();

    let worker_handle = thread::Builder::new()
        .name("buffr-windows-inhibit".into())
        .spawn(move || {
            run_worker(rx, active_worker);
        })
        .map_err(|e| InhibitError::PlatformError(format!("spawn worker: {e}")))?;

    Ok(Box::new(WindowsInhibitor {
        tx,
        active,
        worker: Some(worker_handle),
    }))
}

// ── Worker thread ─────────────────────────────────────────────────────────────

/// All `SetThreadExecutionState` calls happen here, ensuring thread affinity.
fn run_worker(rx: mpsc::Receiver<InhibitCmd>, active: Arc<AtomicBool>) {
    for cmd in rx {
        match cmd {
            InhibitCmd::Acquire => {
                if !active.load(Ordering::Relaxed)
                    && set_execution_state(ES_CONTINUOUS | ES_DISPLAY_REQUIRED)
                {
                    active.store(true, Ordering::Relaxed);
                    tracing::debug!("windows idle inhibitor: acquired");
                }
            }
            InhibitCmd::Release => {
                if active.load(Ordering::Relaxed) {
                    // ES_CONTINUOUS alone clears the sticky display-required flag.
                    let _ = set_execution_state(ES_CONTINUOUS);
                    active.store(false, Ordering::Relaxed);
                    tracing::debug!("windows idle inhibitor: released");
                }
            }
            InhibitCmd::Shutdown => {
                // Release before exiting if still active.
                if active.load(Ordering::Relaxed) {
                    let _ = set_execution_state(ES_CONTINUOUS);
                }
                active.store(false, Ordering::Relaxed);
                tracing::debug!("windows idle inhibitor: worker shut down");
                return;
            }
        }
    }

    // Channel closed without explicit Shutdown — clean up gracefully.
    if active.load(Ordering::Relaxed) {
        let _ = set_execution_state(ES_CONTINUOUS);
    }
    active.store(false, Ordering::Relaxed);
}

// ── Win32 helper ──────────────────────────────────────────────────────────────

/// Call `SetThreadExecutionState` with `flags`.
///
/// Returns `true` on success, logs a warning and returns `false` on failure
/// (return value == 0). Microsoft does not document specific failure modes for
/// this call; failure is rare in practice.
fn set_execution_state(flags: EXECUTION_STATE) -> bool {
    // Safety: `SetThreadExecutionState` is always present in kernel32 on
    // Windows and has no preconditions beyond a valid flags value.
    let prev = unsafe { SetThreadExecutionState(flags) };
    if prev == 0 {
        tracing::warn!(
            "windows idle inhibitor: SetThreadExecutionState returned 0 (flags={flags:#x})"
        );
        false
    } else {
        true
    }
}
