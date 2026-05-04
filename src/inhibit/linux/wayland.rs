//! Wayland idle-inhibit backend using `zwp_idle_inhibit_manager_v1`.
//!
//! ## Connection strategy
//!
//! winit already holds the Wayland connection for rendering. We plug into
//! it by calling `Backend::from_foreign_display` with the `wl_display`
//! pointer from `HasDisplayHandle` — this creates a "guest" backend that
//! shares the same socket without owning it (won't disconnect on drop).
//!
//! The `wl_surface` from `HasWindowHandle` is wrapped with
//! `ObjectId::from_ptr` to get an ID suitable for passing to
//! `create_inhibitor`, without taking ownership of or destroying the surface.
//!
//! ## Thread model
//!
//! A worker thread owns the `Connection`, `EventQueue`, the bound
//! `ZwpIdleInhibitManagerV1`, and the current `ZwpIdleInhibitorV1` (if any).
//! Commands arrive from the UI thread via an `mpsc` channel. `is_active()` is
//! backed by an `AtomicBool` updated by the worker.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender},
    },
    thread,
    time::Duration,
};

use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_registry, wl_surface::WlSurface},
};
use wayland_protocols::wp::idle_inhibit::zv1::client::{
    zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1, zwp_idle_inhibitor_v1::ZwpIdleInhibitorV1,
};
use winit::window::Window;

use super::{IdleInhibitor, InhibitError};

// ── Command channel ───────────────────────────────────────────────────────────

enum InhibitCmd {
    Acquire,
    Release,
    Shutdown,
}

// ── Worker state (Dispatch implementations) ───────────────────────────────────

/// All Wayland state owned by the worker thread.
struct WorkerState {
    inhibitor: Option<ZwpIdleInhibitorV1>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for WorkerState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpIdleInhibitManagerV1, ()> for WorkerState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpIdleInhibitManagerV1,
        _event: <ZwpIdleInhibitManagerV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpIdleInhibitorV1, ()> for WorkerState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpIdleInhibitorV1,
        _event: <ZwpIdleInhibitorV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

// ── Public inhibitor struct ───────────────────────────────────────────────────

/// Wayland idle inhibitor using `zwp_idle_inhibit_manager_v1`.
pub(super) struct WaylandInhibitor {
    tx: SyncSender<InhibitCmd>,
    active: Arc<AtomicBool>,
    /// Join handle so Drop can wait briefly for the worker.
    worker: Option<thread::JoinHandle<()>>,
}

impl std::fmt::Debug for WaylandInhibitor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WaylandInhibitor")
            .field("active", &self.active.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl IdleInhibitor for WaylandInhibitor {
    fn acquire(&self) -> Result<(), InhibitError> {
        if self.active.load(Ordering::Relaxed) {
            return Ok(()); // idempotent
        }
        self.tx
            .send(InhibitCmd::Acquire)
            .map_err(|_| InhibitError::PlatformError("wayland worker thread disconnected".into()))
    }

    fn release(&self) -> Result<(), InhibitError> {
        if !self.active.load(Ordering::Relaxed) {
            return Ok(()); // idempotent
        }
        self.tx
            .send(InhibitCmd::Release)
            .map_err(|_| InhibitError::PlatformError("wayland worker thread disconnected".into()))
    }

    fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }
}

impl Drop for WaylandInhibitor {
    fn drop(&mut self) {
        // Best-effort: ask the worker to release and exit. Ignore send errors
        // (the worker may have already exited on its own).
        let _ = self.tx.send(InhibitCmd::Release);
        let _ = self.tx.send(InhibitCmd::Shutdown);

        // Join the worker, but give it at most 100 ms so we don't stall
        // application shutdown.
        if let Some(handle) = self.worker.take() {
            let join_thread = thread::spawn(move || {
                let _ = handle.join();
            });
            // Sleep the calling thread just long enough for the worker to flush.
            thread::sleep(Duration::from_millis(100));
            drop(join_thread); // if still alive, let it leak — process exiting
        }
    }
}

// ── Constructor ───────────────────────────────────────────────────────────────

/// Construct a [`WaylandInhibitor`] for `window`.
///
/// Returns [`InhibitError::Unsupported`] if the compositor does not
/// advertise `zwp_idle_inhibit_manager_v1`.
pub(super) fn new(window: Arc<Window>) -> Result<Box<dyn IdleInhibitor>, InhibitError> {
    // ── Extract raw Wayland handles ────────────────────────────────────────

    let display_ptr: *mut std::ffi::c_void = {
        let handle = window
            .display_handle()
            .map_err(|e| InhibitError::PlatformError(e.to_string()))?;
        match handle.as_raw() {
            RawDisplayHandle::Wayland(wdh) => wdh.display.as_ptr(),
            other => {
                return Err(InhibitError::PlatformError(format!(
                    "unexpected display handle type: {other:?}"
                )));
            }
        }
    };

    let surface_ptr: *mut std::ffi::c_void = {
        let handle = window
            .window_handle()
            .map_err(|e| InhibitError::PlatformError(e.to_string()))?;
        match handle.as_raw() {
            RawWindowHandle::Wayland(wwh) => wwh.surface.as_ptr(),
            other => {
                return Err(InhibitError::PlatformError(format!(
                    "unexpected window handle type: {other:?}"
                )));
            }
        }
    };

    // ── Build a guest Connection that shares winit's wl_display ───────────

    // Safety: `display_ptr` comes from winit's live Wayland connection and
    // remains valid for the lifetime of `window` (which we hold in an Arc).
    // The guest Backend does NOT disconnect the display on drop.
    let backend = unsafe {
        wayland_backend::client::Backend::from_foreign_display(
            display_ptr as *mut wayland_sys::client::wl_display,
        )
    };
    let conn = Connection::from_backend(backend);

    // ── Bind zwp_idle_inhibit_manager_v1 from the global registry ─────────

    let (globals, event_queue) = registry_queue_init::<WorkerState>(&conn)
        .map_err(|e| InhibitError::PlatformError(format!("wayland registry init: {e}")))?;

    let qh = event_queue.handle();

    let manager: ZwpIdleInhibitManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .map_err(|_| InhibitError::Unsupported)?;

    // ── Wrap winit's wl_surface as a WlSurface proxy ──────────────────────

    // Safety: `surface_ptr` is a valid `wl_proxy*` for a `wl_surface` object
    // that remains alive for as long as the window Arc is live.
    // `ObjectId::from_ptr` records the pointer without taking ownership; the
    // resulting proxy does NOT send `destroy` on drop (no Drop impl exists in
    // the generated code).
    let surface_id = unsafe {
        wayland_backend::client::ObjectId::from_ptr(
            WlSurface::interface(),
            surface_ptr as *mut wayland_sys::client::wl_proxy,
        )
        .map_err(|e| InhibitError::PlatformError(format!("surface ObjectId::from_ptr: {e}")))?
    };

    let wl_surface = WlSurface::from_id(&conn, surface_id)
        .map_err(|e| InhibitError::PlatformError(format!("WlSurface::from_id: {e}")))?;

    // ── Spin up the worker thread ─────────────────────────────────────────

    let (tx, rx) = mpsc::sync_channel::<InhibitCmd>(4);
    let active = Arc::new(AtomicBool::new(false));
    let active_worker = active.clone();

    let worker_handle = thread::Builder::new()
        .name("buffr-wayland-inhibit".into())
        .spawn(move || {
            run_worker(conn, event_queue, manager, wl_surface, rx, active_worker);
        })
        .map_err(|e| InhibitError::PlatformError(format!("spawn worker: {e}")))?;

    Ok(Box::new(WaylandInhibitor {
        tx,
        active,
        worker: Some(worker_handle),
    }))
}

// ── Worker thread ─────────────────────────────────────────────────────────────

/// All Wayland protocol interactions happen here.
fn run_worker(
    conn: Connection,
    mut event_queue: EventQueue<WorkerState>,
    manager: ZwpIdleInhibitManagerV1,
    surface: WlSurface,
    rx: mpsc::Receiver<InhibitCmd>,
    active: Arc<AtomicBool>,
) {
    let qh = event_queue.handle();
    let mut state = WorkerState { inhibitor: None };

    for cmd in rx {
        // Drain pending compositor events (non-blocking) between commands.
        let _ = event_queue.dispatch_pending(&mut state);

        match cmd {
            InhibitCmd::Acquire => {
                if state.inhibitor.is_none() {
                    let inh = manager.create_inhibitor(&surface, &qh, ());
                    let _ = conn.flush();
                    state.inhibitor = Some(inh);
                    active.store(true, Ordering::Relaxed);
                    tracing::debug!("wayland idle inhibitor: acquired");
                }
            }
            InhibitCmd::Release => {
                if let Some(inh) = state.inhibitor.take() {
                    inh.destroy();
                    let _ = conn.flush();
                    active.store(false, Ordering::Relaxed);
                    tracing::debug!("wayland idle inhibitor: released");
                }
            }
            InhibitCmd::Shutdown => {
                if let Some(inh) = state.inhibitor.take() {
                    inh.destroy();
                }
                manager.destroy();
                let _ = conn.flush();
                active.store(false, Ordering::Relaxed);
                tracing::debug!("wayland idle inhibitor: worker shut down");
                return;
            }
        }
    }

    // Channel closed without explicit Shutdown — clean up gracefully.
    if let Some(inh) = state.inhibitor.take() {
        inh.destroy();
    }
    manager.destroy();
    let _ = conn.flush();
    active.store(false, Ordering::Relaxed);
}
