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
//! thread. Commands arrive over the bounded, never-blocking channel owned by
//! the shared `WorkerInhibitor` in `inhibit/mod.rs`.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};

use windows_sys::Win32::System::Power::{
    ES_CONTINUOUS, ES_DISPLAY_REQUIRED, EXECUTION_STATE, SetThreadExecutionState,
};

use super::{IdleInhibitor, InhibitError};
use crate::inhibit::worker::{InhibitCmd, WorkerInhibitor};

// ── Constructor ───────────────────────────────────────────────────────────────

/// Construct a Windows [`WorkerInhibitor`].
///
/// The channel, `active` flag, idempotence checks and shutdown handshake
/// all live in [`WorkerInhibitor`], shared with the Wayland backend; this
/// module only supplies [`run_worker`].
///
/// The pointer arguments are accepted for API symmetry with the Linux
/// backend but are ignored — `SetThreadExecutionState` is per-thread,
/// not per-window.
pub(super) fn new(
    _display_ptr: *mut std::ffi::c_void,
    _surface_ptr: *mut std::ffi::c_void,
) -> Result<Box<dyn IdleInhibitor>, InhibitError> {
    let inhibitor = WorkerInhibitor::spawn("windows", "buffr-windows-inhibit", run_worker)?;
    Ok(Box::new(inhibitor))
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
