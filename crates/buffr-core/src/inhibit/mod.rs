//! Per-platform idle-inhibit (prevent screen lock / display sleep) used
//! by the apps layer when a video is playing in the focused window.
//!
//! Each platform impl is gated behind `cfg(target_os = ...)`. Construction
//! goes through [`new_inhibitor`] which the apps layer calls with the raw
//! `wl_display` + `wl_surface` pointers from the host windowing layer
//! (wayr on Linux; the pointers are ignored on macOS and Windows).
//! [`IdleInhibitor::acquire`] and [`IdleInhibitor::release`] are
//! idempotent — repeated `acquire()` calls are safe.
//!
//! ## Platform status
//!
//! | Platform | Backend | Status |
//! |---|---|---|
//! | Linux (Wayland) | `xdg-session-inhibit` / D-Bus | implemented |
//! | macOS           | `IOPMAssertionCreateWithName` (IOKit) | implemented |
//! | Windows         | `SetThreadExecutionState`      | implemented |
//! | Other           | no-op fallback                 | returns Ok     |

use std::ffi::c_void;

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

/// Construct the platform-default inhibitor.
///
/// `display_ptr` / `surface_ptr` are raw `wl_display*` / `wl_surface*`
/// pointers — typically extracted via `wayr::EventLoop::wl_display_ptr`
/// and `wayr::Toplevel::wl_surface_ptr`. macOS and Windows backends
/// ignore them. The caller MUST keep the underlying objects alive for
/// the inhibitor's lifetime.
///
/// On unsupported platforms returns a no-op inhibitor (an `Ok` variant)
/// rather than an error — callers do not need to special-case platform
/// support.
///
/// # Safety
///
/// On Linux/Wayland: `display_ptr` + `surface_ptr` must point to live
/// objects belonging to the same Wayland connection, and must remain
/// valid for the lifetime of the returned `IdleInhibitor`.
pub unsafe fn new_inhibitor(
    display_ptr: *mut c_void,
    surface_ptr: *mut c_void,
) -> Result<Box<dyn IdleInhibitor>, InhibitError> {
    #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
    {
        // SAFETY: forwarded to the linux backend; same precondition.
        unsafe { linux::new(display_ptr, surface_ptr) }
    }
    #[cfg(target_os = "macos")]
    {
        macos::new(display_ptr, surface_ptr)
    }
    #[cfg(target_os = "windows")]
    {
        windows::new(display_ptr, surface_ptr)
    }
    // Suppress "unused variable" on platforms not covered above.
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = (display_ptr, surface_ptr);
        Ok(Box::new(NoopInhibitor))
    }
}

// ── No-op fallback ────────────────────────────────────────────────────────────

/// No-op fallback for unsupported platforms.
///
/// `acquire` / `release` always return `Ok`; `is_active` is always `false`.
/// Used by Linux as the fall-through when the session is not Wayland,
/// and by the catch-all branch in `new_inhibitor` for unknown OSes.
/// Unused on macOS / Windows (where the dispatcher always picks the real impl)
/// — `allow(dead_code)` keeps `-D warnings` happy on those targets.
#[allow(dead_code)]
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

// ── Shared worker-thread inhibitor ────────────────────────────────────────────

/// Backends that drive a dedicated worker thread. The Wayland and
/// Windows implementations were byte-for-byte identical apart from the
/// log strings and the worker body, so the plumbing lives here once.
#[cfg(any(
    all(target_os = "linux", not(target_env = "ohos")),
    target_os = "windows",
    test
))]
pub(crate) mod worker {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
            mpsc::{self, SyncSender, TrySendError},
        },
        thread,
        time::Duration,
    };

    use super::{IdleInhibitor, InhibitError};

    /// Commands the UI thread posts to a backend worker.
    pub(crate) enum InhibitCmd {
        Acquire,
        Release,
        Shutdown,
    }

    /// Depth of the command channel. Transitions are rare (a video
    /// starting or stopping), so anything queued beyond this means the
    /// worker is wedged and the extra commands are worthless anyway.
    const CMD_CAP: usize = 4;

    /// How long [`WorkerInhibitor::drop`] waits for the worker to exit
    /// before detaching it and letting process exit reap it.
    const SHUTDOWN_GRACE: Duration = Duration::from_millis(100);

    /// Channel + state + shutdown plumbing shared by every worker-thread
    /// idle-inhibit backend. Each backend supplies only its `run_worker`
    /// body via [`WorkerInhibitor::spawn`].
    pub(crate) struct WorkerInhibitor {
        /// Backend name, used verbatim in log lines and error strings
        /// (e.g. `"wayland"`, `"windows"`).
        backend: &'static str,
        tx: SyncSender<InhibitCmd>,
        active: Arc<AtomicBool>,
        /// Join handle so Drop can wait briefly for the worker.
        worker: Option<thread::JoinHandle<()>>,
    }

    impl WorkerInhibitor {
        /// Spawn `run` on a named thread and return the handle wrapping
        /// its command channel.
        ///
        /// `run` receives the command receiver and the shared `active`
        /// flag; it owns every platform object and is the only code
        /// permitted to touch them, which is what gives the backends
        /// their thread affinity.
        pub(crate) fn spawn<F>(
            backend: &'static str,
            thread_name: &str,
            run: F,
        ) -> Result<Self, InhibitError>
        where
            F: FnOnce(mpsc::Receiver<InhibitCmd>, Arc<AtomicBool>) + Send + 'static,
        {
            let (tx, rx) = mpsc::sync_channel::<InhibitCmd>(CMD_CAP);
            let active = Arc::new(AtomicBool::new(false));
            let active_worker = Arc::clone(&active);
            let worker = thread::Builder::new()
                .name(thread_name.to_string())
                .spawn(move || run(rx, active_worker))
                .map_err(|e| InhibitError::PlatformError(format!("spawn worker: {e}")))?;
            Ok(Self {
                backend,
                tx,
                active,
                worker: Some(worker),
            })
        }

        /// Post `cmd` **without ever blocking the caller**.
        ///
        /// `acquire` / `release` run on the winit event loop. A blocking
        /// `SyncSender::send` would park the entire browser UI thread
        /// whenever the worker wedges (a compositor socket that won't
        /// drain, say). A full buffer instead means several transitions
        /// are already queued and this one is stale, so we drop it: the
        /// apps layer re-evaluates inhibit policy every frame against
        /// [`IdleInhibitor::is_active`], so the next frame re-issues
        /// whatever is still needed and the state self-heals.
        fn post(&self, cmd: InhibitCmd, what: &'static str) -> Result<(), InhibitError> {
            match self.tx.try_send(cmd) {
                Ok(()) => Ok(()),
                Err(TrySendError::Full(_)) => {
                    tracing::debug!(
                        backend = self.backend,
                        command = what,
                        "idle inhibitor: worker busy, dropping transition"
                    );
                    Ok(())
                }
                Err(TrySendError::Disconnected(_)) => Err(InhibitError::PlatformError(format!(
                    "{} worker thread disconnected",
                    self.backend
                ))),
            }
        }
    }

    impl std::fmt::Debug for WorkerInhibitor {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("WorkerInhibitor")
                .field("backend", &self.backend)
                .field("active", &self.active.load(Ordering::Relaxed))
                .finish_non_exhaustive()
        }
    }

    impl IdleInhibitor for WorkerInhibitor {
        fn acquire(&self) -> Result<(), InhibitError> {
            if self.active.load(Ordering::Relaxed) {
                return Ok(()); // idempotent
            }
            self.post(InhibitCmd::Acquire, "acquire")
        }

        fn release(&self) -> Result<(), InhibitError> {
            if !self.active.load(Ordering::Relaxed) {
                return Ok(()); // idempotent
            }
            self.post(InhibitCmd::Release, "release")
        }

        fn is_active(&self) -> bool {
            self.active.load(Ordering::Relaxed)
        }
    }

    impl Drop for WorkerInhibitor {
        fn drop(&mut self) {
            // Best-effort: ask the worker to release and exit. These are
            // `try_send` for the same reason `post` is — a wedged worker
            // must not hang application shutdown. If either is dropped,
            // closing `tx` below still ends the worker's `for cmd in rx`
            // loop, and both backends clean up on that path.
            let _ = self.tx.try_send(InhibitCmd::Release);
            let _ = self.tx.try_send(InhibitCmd::Shutdown);

            // Wait for the worker to exit, but give it at most
            // `SHUTDOWN_GRACE` so we don't stall application shutdown.
            // Returns as soon as the worker finishes — only sleeps the
            // full window if it's stuck.
            if let Some(handle) = self.worker.take() {
                let (done_tx, done_rx) = mpsc::sync_channel::<()>(1);
                let spawned = thread::spawn(move || {
                    let _ = handle.join();
                    let _ = done_tx.send(());
                });
                let _ = done_rx.recv_timeout(SHUTDOWN_GRACE);
                // On timeout the watcher detaches naturally; process
                // exit reaps it.
                drop(spawned);
            }
        }
    }
}

// ── Platform stubs ────────────────────────────────────────────────────────────

/// Linux idle-inhibit dispatcher.
///
/// Wayland sessions use `zwp_idle_inhibit_manager_v1` via `wayland-client`.
/// Non-Wayland sessions fall through to `NoopInhibitor` — buffr requires
/// Wayland on Linux, so this path is only reached in headless / CI contexts.
#[cfg(all(target_os = "linux", not(target_env = "ohos")))]
pub mod linux {
    use super::*;

    mod wayland;

    /// Returns true when the running session is Wayland.
    fn is_wayland() -> bool {
        if let Ok(t) = std::env::var("XDG_SESSION_TYPE")
            && t.eq_ignore_ascii_case("wayland")
        {
            return true;
        }
        std::env::var("WAYLAND_DISPLAY").is_ok()
    }

    /// # Safety
    ///
    /// `display_ptr` + `surface_ptr` must point to live `wl_display` /
    /// `wl_surface` objects belonging to the same Wayland connection,
    /// and must remain valid for the returned inhibitor's lifetime.
    pub unsafe fn new(
        display_ptr: *mut c_void,
        surface_ptr: *mut c_void,
    ) -> Result<Box<dyn IdleInhibitor>, InhibitError> {
        if is_wayland() {
            // SAFETY: forwarded to wayland backend with same precondition.
            return unsafe { wayland::new(display_ptr, surface_ptr) };
        }
        let _ = (display_ptr, surface_ptr);
        Ok(Box::new(NoopInhibitor))
    }
}

/// macOS idle-inhibit backend.
///
/// Uses `IOPMAssertionCreateWithName` / `IOPMAssertionRelease` from the
/// `IOKit` framework to prevent display sleep while video is playing.
/// The assertion is process-wide (not per-window); IOKit calls are
/// thread-safe so no worker thread is required.
#[cfg(target_os = "macos")]
pub mod macos;

/// Windows idle-inhibit backend.
///
/// Uses `SetThreadExecutionState(ES_DISPLAY_REQUIRED | ES_CONTINUOUS)` /
/// `SetThreadExecutionState(ES_CONTINUOUS)` from `kernel32` to prevent
/// display sleep. All Win32 calls are made on a dedicated worker thread
/// to preserve thread affinity required by `SetThreadExecutionState`.
#[cfg(target_os = "windows")]
pub mod windows;

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

    // ── Shared worker-thread inhibitor ───────────────────────────────────

    mod worker_inhibitor {
        use super::super::worker::{InhibitCmd, WorkerInhibitor};
        use super::*;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, mpsc};
        use std::time::{Duration, Instant};

        /// Worker that blocks on `gate` before touching the channel, so
        /// tests can wedge it and fill the command buffer on purpose.
        fn wedged_worker(
            gate: mpsc::Receiver<()>,
            seen: Arc<AtomicUsize>,
        ) -> impl FnOnce(mpsc::Receiver<InhibitCmd>, Arc<std::sync::atomic::AtomicBool>) + Send + 'static
        {
            move |rx, _active| {
                // Block until the test releases us.
                let _ = gate.recv();
                for _cmd in rx {
                    seen.fetch_add(1, Ordering::SeqCst);
                }
            }
        }

        #[test]
        fn acquire_never_blocks_when_the_worker_is_wedged() {
            // Regression (M36): `SyncSender::send` from the winit event
            // loop parked the whole browser UI thread once the 4-slot
            // buffer filled behind a stuck worker.
            let (gate_tx, gate_rx) = mpsc::channel::<()>();
            let seen = Arc::new(AtomicUsize::new(0));
            let inhibitor = WorkerInhibitor::spawn(
                "test",
                "buffr-test-inhibit",
                wedged_worker(gate_rx, Arc::clone(&seen)),
            )
            .unwrap();

            // Far more transitions than the channel can hold. Every one
            // must return promptly rather than parking.
            let start = Instant::now();
            for _ in 0..1000 {
                inhibitor.acquire().unwrap();
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "acquire blocked on a full channel"
            );

            // Overflow is dropped, not queued: the worker cannot have
            // received more than the buffer depth.
            drop(gate_tx);
            drop(inhibitor);
            assert!(
                seen.load(Ordering::SeqCst) <= 8,
                "commands were not dropped"
            );
        }

        #[test]
        fn drop_does_not_hang_on_a_wedged_worker() {
            let (gate_tx, gate_rx) = mpsc::channel::<()>();
            let seen = Arc::new(AtomicUsize::new(0));
            let inhibitor = WorkerInhibitor::spawn(
                "test",
                "buffr-test-inhibit-drop",
                wedged_worker(gate_rx, seen),
            )
            .unwrap();
            for _ in 0..64 {
                inhibitor.acquire().unwrap();
            }
            let start = Instant::now();
            drop(inhibitor);
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "Drop blocked on a wedged worker"
            );
            drop(gate_tx);
        }

        #[test]
        fn commands_reach_a_healthy_worker_and_flip_active() {
            let flipped = Arc::new(AtomicUsize::new(0));
            let flipped_w = Arc::clone(&flipped);
            let inhibitor =
                WorkerInhibitor::spawn("test", "buffr-test-inhibit-ok", move |rx, active| {
                    for cmd in rx {
                        match cmd {
                            InhibitCmd::Acquire => {
                                active.store(true, Ordering::Relaxed);
                                flipped_w.fetch_add(1, Ordering::SeqCst);
                            }
                            InhibitCmd::Release => {
                                active.store(false, Ordering::Relaxed);
                                flipped_w.fetch_add(1, Ordering::SeqCst);
                            }
                            InhibitCmd::Shutdown => return,
                        }
                    }
                })
                .unwrap();

            assert!(!inhibitor.is_active());
            inhibitor.acquire().unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while !inhibitor.is_active() && Instant::now() < deadline {
                std::thread::yield_now();
            }
            assert!(inhibitor.is_active(), "worker never applied Acquire");

            // Idempotent: a second acquire is a pure no-op.
            let before = flipped.load(Ordering::SeqCst);
            inhibitor.acquire().unwrap();
            assert_eq!(flipped.load(Ordering::SeqCst), before);

            inhibitor.release().unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while inhibitor.is_active() && Instant::now() < deadline {
                std::thread::yield_now();
            }
            assert!(!inhibitor.is_active(), "worker never applied Release");
        }

        #[test]
        fn disconnected_worker_reports_a_platform_error() {
            let inhibitor =
                WorkerInhibitor::spawn("test", "buffr-test-inhibit-dead", |rx, active| {
                    // Exit immediately, dropping the receiver.
                    drop(rx);
                    active.store(true, std::sync::atomic::Ordering::Relaxed);
                })
                .unwrap();
            // Wait for the receiver to actually be gone.
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut err = None;
            while Instant::now() < deadline {
                match inhibitor.release() {
                    Ok(()) => std::thread::yield_now(),
                    Err(e) => {
                        err = Some(e);
                        break;
                    }
                }
            }
            let err = err.expect("expected a disconnect error");
            assert!(matches!(err, InhibitError::PlatformError(ref m) if m.contains("test")));
        }
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
