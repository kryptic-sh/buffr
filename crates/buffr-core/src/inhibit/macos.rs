//! macOS idle-inhibit backend using `IOPMAssertionCreateWithName`.
//!
//! ## API
//!
//! `IOPMAssertionCreateWithName` / `IOPMAssertionRelease` from the `IOKit`
//! framework prevent display sleep while video is playing. The assertion is
//! process-wide (not per-window), so the `Arc<Window>` parameter is accepted
//! for API symmetry but is not used.
//!
//! ## Thread model
//!
//! IOKit PM assertion calls are thread-safe and synchronous. No worker thread
//! is needed — acquire and release call IOKit directly from whatever thread
//! the apps layer uses.

use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

// `TCFType` brings `as_CFTypeRef` into scope. The `as_concrete_TypeRef`
// method on this trait is unreliable when multiple core-foundation versions
// coexist in the dep tree (wgpu pulls 0.9; we use 0.10) — fall back to
// `as_CFTypeRef` + a pointer cast.
#[allow(unused_imports)]
use core_foundation::base::TCFType;
use core_foundation::string::{CFString, CFStringRef};

use super::{IdleInhibitor, InhibitError};

// ── IOKit FFI ─────────────────────────────────────────────────────────────────

/// Opaque assertion ID returned by IOKit (typedef'd to `u32` in IOKit headers).
type IOPMAssertionID = u32;

/// Return code from IOKit calls. `kIOReturnSuccess` == 0.
type IOReturn = i32;

const K_IOPM_ASSERTION_LEVEL_ON: u32 = 255;
const K_IORETURN_SUCCESS: IOReturn = 0;

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPMAssertionCreateWithName(
        assertion_type: CFStringRef,
        assertion_level: u32,
        assertion_name: CFStringRef,
        assertion_id: *mut IOPMAssertionID,
    ) -> IOReturn;

    fn IOPMAssertionRelease(assertion_id: IOPMAssertionID) -> IOReturn;
}

// ── Inhibitor struct ──────────────────────────────────────────────────────────

/// macOS idle inhibitor using `IOPMAssertionCreateWithName`.
pub(super) struct MacosInhibitor {
    /// Holds the current assertion ID when active.
    state: Mutex<Option<IOPMAssertionID>>,
    /// Cached active flag for cheap `is_active()` reads without locking.
    active: AtomicBool,
}

impl std::fmt::Debug for MacosInhibitor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MacosInhibitor")
            .field("active", &self.active.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl MacosInhibitor {
    fn new() -> Self {
        Self {
            state: Mutex::new(None),
            active: AtomicBool::new(false),
        }
    }

    /// Call `IOPMAssertionRelease` for `id`, logging a warning on failure.
    fn release_id(id: IOPMAssertionID) {
        let ret = unsafe { IOPMAssertionRelease(id) };
        if ret != K_IORETURN_SUCCESS {
            tracing::warn!(
                "macos idle inhibitor: IOPMAssertionRelease failed: {ret} (best-effort, ignored)"
            );
        } else {
            tracing::debug!("macos idle inhibitor: released (id={id})");
        }
    }
}

impl IdleInhibitor for MacosInhibitor {
    fn acquire(&self) -> Result<(), InhibitError> {
        // Fast path: already active — avoid locking.
        if self.active.load(Ordering::Relaxed) {
            return Ok(());
        }

        let mut guard = self.state.lock().unwrap_or_else(|p| p.into_inner());

        // Re-check under the lock (another thread may have raced).
        if guard.is_some() {
            return Ok(());
        }

        let assertion_type = CFString::new("NoDisplaySleepAssertion");
        let assertion_name = CFString::new("buffr video playback");

        let mut id: IOPMAssertionID = 0;
        // Two `core-foundation` versions coexist in the dep tree (0.9 from
        // wgpu/winit, 0.10 from us), which makes `TCFType::as_concrete_TypeRef`
        // resolution fail on macOS even with the trait imported. Use
        // `as_CFTypeRef` (returns CFTypeRef = *const c_void) and cast to
        // CFStringRef = *const __CFString — same opaque pointer at runtime.
        let ret = unsafe {
            IOPMAssertionCreateWithName(
                assertion_type.as_CFTypeRef() as CFStringRef,
                K_IOPM_ASSERTION_LEVEL_ON,
                assertion_name.as_CFTypeRef() as CFStringRef,
                &mut id,
            )
        };

        if ret != K_IORETURN_SUCCESS {
            return Err(InhibitError::PlatformError(format!(
                "IOPMAssertionCreateWithName failed: {ret}"
            )));
        }

        *guard = Some(id);
        self.active.store(true, Ordering::Relaxed);
        tracing::debug!("macos idle inhibitor: acquired (id={id})");
        Ok(())
    }

    fn release(&self) -> Result<(), InhibitError> {
        // Fast path: not active — avoid locking.
        if !self.active.load(Ordering::Relaxed) {
            return Ok(());
        }

        let mut guard = self.state.lock().unwrap_or_else(|p| p.into_inner());

        if let Some(id) = guard.take() {
            Self::release_id(id);
            self.active.store(false, Ordering::Relaxed);
        }
        Ok(())
    }

    fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }
}

impl Drop for MacosInhibitor {
    fn drop(&mut self) {
        // Release any outstanding assertion without returning an error.
        let mut guard = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(id) = guard.take() {
            Self::release_id(id);
            self.active.store(false, Ordering::Relaxed);
        }
    }
}

// ── Constructor ───────────────────────────────────────────────────────────────

/// Construct a [`MacosInhibitor`].
///
/// The pointer arguments are accepted for API symmetry with the Linux
/// backend but are ignored — `IOPMAssertion` is process-wide, not
/// per-window.
pub(super) fn new(
    _display_ptr: *mut std::ffi::c_void,
    _surface_ptr: *mut std::ffi::c_void,
) -> Result<Box<dyn IdleInhibitor>, InhibitError> {
    Ok(Box::new(MacosInhibitor::new()))
}
