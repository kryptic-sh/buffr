//! Per-platform idle-inhibit (prevent screen lock / display sleep) used
//! by the apps layer when a video is playing in the focused window.
//!
//! Each platform impl is gated behind `cfg(target_os = ...)`. Construction
//! goes through [`new_inhibitor`] which the apps layer calls with the active
//! winit window. [`IdleInhibitor::acquire`] and [`IdleInhibitor::release`]
//! are idempotent — repeated `acquire()` calls are safe.
//!
//! ## Platform status
//!
//! | Platform | Backend | Status |
//! |---|---|---|
//! | Linux (Wayland) | `xdg-session-inhibit` / D-Bus | stub (phase-2) |
//! | Linux (X11)     | `XSS_SUSPEND_SCREENSAVER`     | stub (phase-2) |
//! | macOS           | `IOPMAssertionCreateWithName`  | stub (phase-2) |
//! | Windows         | `SetThreadExecutionState`      | stub (phase-2) |
//! | Other           | no-op fallback                 | returns Ok     |

use std::sync::Arc;

use winit::window::Window;

/// Errors that idle-inhibit operations can produce.
#[derive(Debug)]
pub enum InhibitError {
    /// The current platform or session type does not support idle inhibit.
    Unsupported,
    /// A platform-specific error occurred (D-Bus, IOKit, Win32, …).
    PlatformError(String),
}

impl std::fmt::Display for InhibitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InhibitError::Unsupported => write!(f, "idle inhibit not supported on this platform"),
            InhibitError::PlatformError(msg) => {
                write!(f, "idle inhibit platform error: {msg}")
            }
        }
    }
}

impl std::error::Error for InhibitError {}

/// Per-window idle inhibitor. Drop releases automatically.
///
/// Implementors must be `Send + Sync` so the apps layer can hold the
/// `Box<dyn IdleInhibitor>` inside `AppState` without additional locking.
pub trait IdleInhibitor: Send + Sync {
    /// Activate the inhibitor. Idempotent — calling `acquire` when already
    /// active is a no-op.
    fn acquire(&self) -> Result<(), InhibitError>;

    /// Release the inhibitor. Idempotent — calling `release` when not
    /// active is a no-op. Must be called explicitly or via `Drop`.
    fn release(&self) -> Result<(), InhibitError>;

    /// Current state, for diagnostics.
    fn is_active(&self) -> bool;
}

/// Construct the platform-default inhibitor for `window`.
///
/// On unsupported platforms returns a no-op inhibitor (an `Ok` variant)
/// rather than an error — callers do not need to special-case platform
/// support. On supported platforms the current stubs also return the
/// no-op inhibitor until the platform agents fill in the real impl.
pub fn new_inhibitor(window: Arc<Window>) -> Result<Box<dyn IdleInhibitor>, InhibitError> {
    #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
    return linux::new(window);
    #[cfg(target_os = "macos")]
    return macos::new(window);
    #[cfg(target_os = "windows")]
    return windows::new(window);
    // Suppress "unused variable" on platforms not covered above.
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = window;
        Ok(Box::new(NoopInhibitor))
    }
}

// ── No-op fallback ────────────────────────────────────────────────────────────

/// No-op fallback for unsupported platforms.
///
/// `acquire` / `release` always return `Ok`; `is_active` is always `false`.
/// Used by platform stubs until the real impl lands.
#[derive(Debug, Default)]
pub(crate) struct NoopInhibitor;

impl IdleInhibitor for NoopInhibitor {
    fn acquire(&self) -> Result<(), InhibitError> {
        Ok(())
    }

    fn release(&self) -> Result<(), InhibitError> {
        Ok(())
    }

    fn is_active(&self) -> bool {
        false
    }
}

// ── Platform stubs ────────────────────────────────────────────────────────────

/// Linux idle-inhibit stub.
///
/// TODO (phase-2): Detect session type at runtime and pick the right backend:
///   - Check `WAYLAND_DISPLAY` (or `XDG_SESSION_TYPE == "wayland"`) first.
///   - If Wayland: use the `org.freedesktop.ScreenSaver.Inhibit` D-Bus method
///     (or the zwp_idle_inhibit_manager_v1 Wayland protocol if the compositor
///     exposes it).
///   - Fallback to X11: use `XSS_SUSPEND_SCREENSAVER` via `XScreenSaverSuspend`
///     (requires `libXss`).
///
/// For now returns `NoopInhibitor` on both Wayland and X11 sessions.
#[cfg(all(target_os = "linux", not(target_env = "ohos")))]
pub mod linux {
    use super::*;

    pub fn new(_window: Arc<Window>) -> Result<Box<dyn IdleInhibitor>, InhibitError> {
        // TODO: check XDG_SESSION_TYPE / WAYLAND_DISPLAY and dispatch to
        // wayland::new() or x11::new() once those sub-backends land.
        Ok(Box::new(NoopInhibitor))
    }
}

/// macOS idle-inhibit stub.
///
/// TODO (phase-2): Use `IOPMAssertionCreateWithName` /
/// `IOPMAssertionRelease` from the `IOKit` framework to prevent display
/// sleep while video is playing.
#[cfg(target_os = "macos")]
pub mod macos {
    use super::*;

    pub fn new(_window: Arc<Window>) -> Result<Box<dyn IdleInhibitor>, InhibitError> {
        Ok(Box::new(NoopInhibitor))
    }
}

/// Windows idle-inhibit stub.
///
/// TODO (phase-2): Use `SetThreadExecutionState(ES_DISPLAY_REQUIRED |
/// ES_CONTINUOUS)` / `SetThreadExecutionState(ES_CONTINUOUS)` from
/// `kernel32` to inhibit display sleep.
#[cfg(target_os = "windows")]
pub mod windows {
    use super::*;

    pub fn new(_window: Arc<Window>) -> Result<Box<dyn IdleInhibitor>, InhibitError> {
        Ok(Box::new(NoopInhibitor))
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_acquire_returns_ok() {
        let inhibitor = NoopInhibitor;
        assert!(inhibitor.acquire().is_ok());
    }

    #[test]
    fn noop_is_active_always_false() {
        let inhibitor = NoopInhibitor;
        inhibitor.acquire().unwrap();
        assert!(!inhibitor.is_active());
    }

    #[test]
    fn noop_release_returns_ok() {
        let inhibitor = NoopInhibitor;
        assert!(inhibitor.release().is_ok());
    }

    #[test]
    fn noop_repeated_acquire_ok() {
        let inhibitor = NoopInhibitor;
        assert!(inhibitor.acquire().is_ok());
        assert!(inhibitor.acquire().is_ok());
        assert!(!inhibitor.is_active());
    }

    #[test]
    fn noop_release_when_not_acquired_ok() {
        let inhibitor = NoopInhibitor;
        assert!(inhibitor.release().is_ok());
        assert!(!inhibitor.is_active());
    }

    /// Demonstrate the inhibit-policy evaluation in pure logic (no winit
    /// window needed). Mirrors the decision the apps layer makes in
    /// `about_to_wait`.
    ///
    /// Policy: want_inhibit = enabled && (video_active || (inhibit_audio_only && media_active))
    ///                                 && (!require_focus || focused)
    fn want_inhibit(
        enabled: bool,
        video_active: bool,
        media_active: bool,
        inhibit_audio_only: bool,
        require_focus: bool,
        focused: bool,
    ) -> bool {
        if !enabled {
            return false;
        }
        let signal = video_active || (inhibit_audio_only && media_active);
        let focus_ok = !require_focus || focused;
        signal && focus_ok
    }

    #[test]
    fn policy_video_and_focused_wants_inhibit() {
        assert!(want_inhibit(true, true, false, false, true, true));
    }

    #[test]
    fn policy_video_unfocused_no_inhibit_when_require_focus() {
        assert!(!want_inhibit(true, true, false, false, true, false));
    }

    #[test]
    fn policy_video_unfocused_inhibits_when_require_focus_false() {
        assert!(want_inhibit(true, true, false, false, false, false));
    }

    #[test]
    fn policy_audio_only_no_inhibit_by_default() {
        // video_active=false, media_active=true, inhibit_audio_only=false
        assert!(!want_inhibit(true, false, true, false, true, true));
    }

    #[test]
    fn policy_audio_only_inhibits_when_flag_set() {
        // video_active=false, media_active=true, inhibit_audio_only=true
        assert!(want_inhibit(true, false, true, true, true, true));
    }

    #[test]
    fn policy_disabled_never_inhibits() {
        assert!(!want_inhibit(false, true, true, true, false, true));
    }

    #[test]
    fn policy_no_signal_no_inhibit() {
        assert!(!want_inhibit(true, false, false, false, false, true));
    }

    #[test]
    fn inhibit_error_display_unsupported() {
        let msg = InhibitError::Unsupported.to_string();
        assert!(msg.contains("not supported"));
    }

    #[test]
    fn inhibit_error_display_platform_error() {
        let msg = InhibitError::PlatformError("dbus down".into()).to_string();
        assert!(msg.contains("dbus down"));
    }
}
