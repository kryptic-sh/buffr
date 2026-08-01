//! Wayland idle-inhibit backend using `zwp_idle_inhibit_manager_v1`.
//!
//! ## Connection strategy
//!
//! The host application (buffr-app via wayr) already holds the Wayland
//! connection for rendering. We plug into it by calling
//! `Backend::from_foreign_display` with the raw `wl_display` pointer the
//! caller hands in — this creates a "guest" backend that shares the same
//! socket without owning it (won't disconnect on drop).
//!
//! The host's `wl_surface` pointer is wrapped with `ObjectId::from_ptr`
//! to get an ID suitable for passing to `create_inhibitor`, without
//! taking ownership of or destroying the surface.
//!
//! ## Thread model
//!
//! A worker thread owns the `Connection`, `EventQueue`, the bound
//! `ZwpIdleInhibitManagerV1`, and the current `ZwpIdleInhibitorV1` (if any).
//! Commands arrive from the UI thread over the bounded, never-blocking channel
//! owned by the shared `WorkerInhibitor` in `inhibit/mod.rs`, which also carries
//! the `AtomicBool` backing `is_active()`.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};

use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_registry, wl_surface::WlSurface},
};
use wayland_protocols::wp::idle_inhibit::zv1::client::{
    zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1, zwp_idle_inhibitor_v1::ZwpIdleInhibitorV1,
};

use super::{IdleInhibitor, InhibitError};
use crate::inhibit::worker::{InhibitCmd, WorkerInhibitor};

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

// ── Constructor ───────────────────────────────────────────────────────────────

/// Construct a Wayland [`WorkerInhibitor`] from raw `wl_display` + `wl_surface`
/// pointers handed in by the host. The caller is responsible for keeping
/// both objects alive for the inhibitor's lifetime — typically the host's
/// `wayr::Toplevel` owns the `wl_surface` and `wayr::EventLoop` owns the
/// `wl_display`, and the inhibitor drops before either.
///
/// Returns [`InhibitError::Unsupported`] if the compositor does not
/// advertise `zwp_idle_inhibit_manager_v1`.
///
/// # Safety
///
/// `display_ptr` must point to a live `wl_display`; `surface_ptr` must
/// point to a live `wl_surface` on that display. Both must remain valid
/// for the lifetime of the returned `IdleInhibitor`.
pub(super) unsafe fn new(
    display_ptr: *mut std::ffi::c_void,
    surface_ptr: *mut std::ffi::c_void,
) -> Result<Box<dyn IdleInhibitor>, InhibitError> {
    if display_ptr.is_null() {
        return Err(InhibitError::PlatformError(
            "wl_display pointer is null".into(),
        ));
    }
    if surface_ptr.is_null() {
        return Err(InhibitError::PlatformError(
            "wl_surface pointer is null".into(),
        ));
    }

    // ── Build a guest Connection that shares the host's wl_display ────────

    // Safety: caller guarantees the pointer is live; the guest Backend
    // does NOT disconnect the display on drop.
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

    // ── Wrap the host's wl_surface as a WlSurface proxy ───────────────────

    // Safety: `surface_ptr` is a valid `wl_proxy*` for a `wl_surface` object
    // that remains alive for as long as the caller keeps the host window.
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

    let inhibitor =
        WorkerInhibitor::spawn("wayland", "buffr-wayland-inhibit", move |rx, active| {
            run_worker(conn, event_queue, manager, wl_surface, rx, active);
        })?;
    Ok(Box::new(inhibitor))
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
