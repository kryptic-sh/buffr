//! X11 idle-inhibit backend using `org.freedesktop.ScreenSaver` D-Bus.
//!
//! ## Why D-Bus over XScreenSaverSuspend
//!
//! The older XSS approach requires linking `libXss` and only reaches the X11
//! screen-saver extension. The D-Bus `org.freedesktop.ScreenSaver` interface
//! is implemented by every major DE (GNOME, KDE, XFCE, LightDM …) and is the
//! same path used by e.g. Firefox and VLC on X11.
//!
//! ## Thread model
//!
//! A dedicated worker thread owns the `zbus::blocking::Connection` and the
//! current inhibit cookie (if any). Commands arrive from the UI thread via an
//! `mpsc::sync_channel(4)`. `is_active()` reads an `AtomicBool` updated by
//! the worker after each state change.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender},
    },
    thread,
    time::Duration,
};

use winit::window::Window;

use super::{IdleInhibitor, InhibitError};

// ── D-Bus constants ───────────────────────────────────────────────────────────

const DBUS_DEST: &str = "org.freedesktop.ScreenSaver";
const DBUS_PATH: &str = "/org/freedesktop/ScreenSaver";
const DBUS_IFACE: &str = "org.freedesktop.ScreenSaver";
const APP_NAME: &str = "buffr";
const INHIBIT_REASON: &str = "Video playback";

// ── Command channel ───────────────────────────────────────────────────────────

enum InhibitCmd {
    Acquire,
    Release,
    Shutdown,
}

// ── Public inhibitor struct ───────────────────────────────────────────────────

/// X11 idle inhibitor using `org.freedesktop.ScreenSaver` D-Bus.
pub(super) struct X11Inhibitor {
    tx: SyncSender<InhibitCmd>,
    active: Arc<AtomicBool>,
    /// Join handle so Drop can wait briefly for the worker.
    worker: Option<thread::JoinHandle<()>>,
}

impl std::fmt::Debug for X11Inhibitor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("X11Inhibitor")
            .field("active", &self.active.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl IdleInhibitor for X11Inhibitor {
    fn acquire(&self) -> Result<(), InhibitError> {
        if self.active.load(Ordering::Relaxed) {
            return Ok(()); // idempotent
        }
        self.tx
            .send(InhibitCmd::Acquire)
            .map_err(|_| InhibitError::PlatformError("x11 worker thread disconnected".into()))
    }

    fn release(&self) -> Result<(), InhibitError> {
        if !self.active.load(Ordering::Relaxed) {
            return Ok(()); // idempotent
        }
        self.tx
            .send(InhibitCmd::Release)
            .map_err(|_| InhibitError::PlatformError("x11 worker thread disconnected".into()))
    }

    fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }
}

impl Drop for X11Inhibitor {
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

/// Construct an [`X11Inhibitor`] for `window`.
///
/// The `window` parameter is accepted for API symmetry with other backends
/// but is not used — the D-Bus `org.freedesktop.ScreenSaver` inhibit is
/// screen-wide, not per-window.
///
/// Returns [`InhibitError::Unsupported`] if `org.freedesktop.ScreenSaver` is
/// not present on the session bus (e.g. headless X11 without a DE).
pub(super) fn new(_window: Arc<Window>) -> Result<Box<dyn IdleInhibitor>, InhibitError> {
    // Probe the session bus and verify the ScreenSaver service exists.
    let conn = zbus::blocking::Connection::session().map_err(|_| InhibitError::Unsupported)?;

    // Verify the destination is reachable before spawning the worker.
    // A NameHasNoOwner / ServiceUnknown reply means no DE screensaver is running.
    conn.call_method(
        Some(DBUS_DEST),
        DBUS_PATH,
        Some("org.freedesktop.DBus.Peer"),
        "Ping",
        &(),
    )
    .map_err(|_| InhibitError::Unsupported)?;

    // ── Spin up the worker thread ─────────────────────────────────────────

    let (tx, rx) = mpsc::sync_channel::<InhibitCmd>(4);
    let active = Arc::new(AtomicBool::new(false));
    let active_worker = active.clone();

    let worker_handle = thread::Builder::new()
        .name("buffr-x11-inhibit".into())
        .spawn(move || {
            run_worker(conn, rx, active_worker);
        })
        .map_err(|e| InhibitError::PlatformError(format!("spawn worker: {e}")))?;

    Ok(Box::new(X11Inhibitor {
        tx,
        active,
        worker: Some(worker_handle),
    }))
}

// ── Worker thread ─────────────────────────────────────────────────────────────

/// All D-Bus interactions happen here.
fn run_worker(
    conn: zbus::blocking::Connection,
    rx: mpsc::Receiver<InhibitCmd>,
    active: Arc<AtomicBool>,
) {
    let mut cookie: Option<u32> = None;

    for cmd in rx {
        match cmd {
            InhibitCmd::Acquire => {
                if cookie.is_none() {
                    match dbus_inhibit(&conn) {
                        Ok(c) => {
                            cookie = Some(c);
                            active.store(true, Ordering::Relaxed);
                            tracing::debug!("x11 idle inhibitor: acquired (cookie={c})");
                        }
                        Err(e) => {
                            tracing::debug!("x11 idle inhibitor: acquire failed: {e}");
                        }
                    }
                }
            }
            InhibitCmd::Release => {
                if let Some(c) = cookie.take() {
                    if let Err(e) = dbus_uninhibit(&conn, c) {
                        tracing::debug!("x11 idle inhibitor: uninhibit failed: {e}");
                    } else {
                        tracing::debug!("x11 idle inhibitor: released (cookie={c})");
                    }
                    active.store(false, Ordering::Relaxed);
                }
            }
            InhibitCmd::Shutdown => {
                // Release before exiting if still active.
                if let Some(c) = cookie.take() {
                    let _ = dbus_uninhibit(&conn, c);
                }
                active.store(false, Ordering::Relaxed);
                tracing::debug!("x11 idle inhibitor: worker shut down");
                return;
            }
        }
    }

    // Channel closed without explicit Shutdown — clean up gracefully.
    if let Some(c) = cookie.take() {
        let _ = dbus_uninhibit(&conn, c);
    }
    active.store(false, Ordering::Relaxed);
}

// ── D-Bus helpers ─────────────────────────────────────────────────────────────

/// Call `org.freedesktop.ScreenSaver.Inhibit` and return the cookie.
fn dbus_inhibit(conn: &zbus::blocking::Connection) -> Result<u32, InhibitError> {
    let reply = conn
        .call_method(
            Some(DBUS_DEST),
            DBUS_PATH,
            Some(DBUS_IFACE),
            "Inhibit",
            &(APP_NAME, INHIBIT_REASON),
        )
        .map_err(|e| InhibitError::PlatformError(format!("ScreenSaver.Inhibit: {e}")))?;

    reply
        .body()
        .deserialize::<u32>()
        .map_err(|e| InhibitError::PlatformError(format!("Inhibit cookie deserialize: {e}")))
}

/// Call `org.freedesktop.ScreenSaver.UnInhibit` with the stored cookie.
fn dbus_uninhibit(conn: &zbus::blocking::Connection, cookie: u32) -> Result<(), InhibitError> {
    conn.call_method(
        Some(DBUS_DEST),
        DBUS_PATH,
        Some(DBUS_IFACE),
        "UnInhibit",
        &(cookie,),
    )
    .map_err(|e| InhibitError::PlatformError(format!("ScreenSaver.UnInhibit: {e}")))?;
    Ok(())
}
