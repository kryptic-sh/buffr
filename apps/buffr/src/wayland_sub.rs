//! `wl_subsurface` for the buffr statusline — Phase 0+1+2 PoC.
//!
//! The compositor (Hyprland / wlroots) edge-clamps the parent toplevel's
//! buffer during a top-edge live-drag: as the window grows the compositor
//! replaces the gap with the bottom row of the stale wgpu buffer. Moving
//! the statusline into a `wl_subsurface` in `desync` mode decouples it
//! completely from the parent surface's commit cadence, so it stays
//! pixel-perfectly anchored to the window bottom even while the GPU
//! buffer hasn't caught up yet.
//!
//! # Platform gating
//!
//! The entire real implementation is behind `#[cfg(target_os = "linux")]`.
//! On other platforms (macOS, Windows) `WaylandSub` is a zero-sized stub
//! whose methods compile away. Callers are platform-oblivious.

// ---- non-Linux stub -------------------------------------------------------

#[cfg(not(target_os = "linux"))]
pub struct WaylandSub;

#[cfg(not(target_os = "linux"))]
impl WaylandSub {
    pub fn new(_window: &winit::window::Window) -> Option<Self> {
        None
    }

    pub fn set_size(&mut self, _window_w: u32, _window_h: u32) {}

    pub fn paint<F: FnOnce(&mut [u32], usize, usize)>(&mut self, _paint_fn: F) {}

    pub fn flush(&mut self) {}
}

// ---- Linux implementation -------------------------------------------------

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::c_void;
    use std::os::unix::io::OwnedFd;

    use rustix::fs::{MemfdFlags, ftruncate, memfd_create};
    use rustix::mm::{MapFlags, ProtFlags, mmap};
    use tracing::{debug, info, warn};
    use wayland_client::{
        Connection, Dispatch, EventQueue, Proxy, QueueHandle,
        backend::Backend,
        globals::{GlobalList, GlobalListContents, registry_queue_init},
        protocol::{
            wl_buffer, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_subcompositor,
            wl_subsurface, wl_surface,
        },
    };

    use buffr_ui::STATUSLINE_HEIGHT;

    // ---- State struct for our private event queue -------------------------

    struct WlState {
        compositor: Option<wl_compositor::WlCompositor>,
        subcompositor: Option<wl_subcompositor::WlSubcompositor>,
        shm: Option<wl_shm::WlShm>,
    }

    impl WlState {
        fn new() -> Self {
            Self {
                compositor: None,
                subcompositor: None,
                shm: None,
            }
        }
    }

    // ---- Dispatch impls for WlState ---------------------------------------
    //
    // We only need the globals; all actual protocol objects we create in
    // desync mode don't generate meaningful events for this PoC.

    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for WlState {
        fn event(
            _state: &mut Self,
            _proxy: &wl_registry::WlRegistry,
            _event: wl_registry::Event,
            _data: &GlobalListContents,
            _conn: &Connection,
            _qh: &QueueHandle<Self>,
        ) {
            // Dynamic globals arrive here; we don't need them post-init.
        }
    }

    // No-event impls for objects we create:
    wayland_client::delegate_noop!(WlState: ignore wl_compositor::WlCompositor);
    wayland_client::delegate_noop!(WlState: ignore wl_subcompositor::WlSubcompositor);
    wayland_client::delegate_noop!(WlState: ignore wl_shm::WlShm);
    wayland_client::delegate_noop!(WlState: ignore wl_shm_pool::WlShmPool);
    wayland_client::delegate_noop!(WlState: ignore wl_surface::WlSurface);
    wayland_client::delegate_noop!(WlState: ignore wl_subsurface::WlSubsurface);
    wayland_client::delegate_noop!(WlState: ignore wl_buffer::WlBuffer);

    // ---- Shm pool + double-buffered slots --------------------------------
    //
    // Two slots, alternated per paint to avoid overwriting a buffer the
    // compositor may still be reading. The pool is sized for 2 × stride ×
    // height bytes; slot 0 starts at offset 0, slot 1 at offset
    // (stride × height). On every paint we flip `next_slot`, write into
    // the now-free slot, and attach that slot's wl_buffer.

    struct ShmPool {
        /// The anonymous file backing the shm pool.
        fd: OwnedFd,
        /// Mapped memory as raw pointer. Length = capacity (bytes).
        ptr: *mut c_void,
        /// Capacity in bytes (allocated). Always 2 × per-slot size.
        capacity: usize,
        /// wl_shm_pool handle.
        pool: wl_shm_pool::WlShmPool,
        /// Current width (px) of each slot's buffer.
        width: u32,
        /// Two wl_buffer wrappers, one per slot. Both share the same pool.
        buffers: [Option<wl_buffer::WlBuffer>; 2],
        /// Index of the slot to paint into next (0 or 1).
        next_slot: usize,
    }

    // SAFETY: the mmap'd pointer is only touched from the main thread.
    unsafe impl Send for ShmPool {}

    impl ShmPool {
        /// Bytes per slot for a given width.
        fn slot_bytes(width: u32) -> usize {
            (width * STATUSLINE_HEIGHT * 4) as usize
        }

        /// Create a new shm pool with capacity for two slots of `slot_bytes`.
        fn new(shm: &wl_shm::WlShm, qh: &QueueHandle<WlState>, slot_bytes: usize) -> Option<Self> {
            let total = slot_bytes * 2;
            let fd = memfd_create("buffr-statusline-shm", MemfdFlags::CLOEXEC)
                .map_err(|e| warn!(?e, "memfd_create failed"))
                .ok()?;
            ftruncate(&fd, total as u64)
                .map_err(|e| warn!(?e, "ftruncate failed"))
                .ok()?;
            // SAFETY: fd is a valid memfd sized to total bytes.
            let ptr = unsafe {
                mmap(
                    std::ptr::null_mut(),
                    total,
                    ProtFlags::READ | ProtFlags::WRITE,
                    MapFlags::SHARED,
                    &fd,
                    0,
                )
            }
            .map_err(|e| warn!(?e, "mmap failed"))
            .ok()?;

            use std::os::unix::io::AsFd;
            let pool = shm.create_pool(fd.as_fd(), total as i32, qh, ());
            debug!(bytes = total, "shm pool created (double-buffered)");

            Some(Self {
                fd,
                ptr,
                capacity: total,
                pool,
                width: 0,
                buffers: [None, None],
                next_slot: 0,
            })
        }

        /// Grow the pool if `needed_slot_bytes × 2` exceeds the current capacity.
        /// Recreates the pool entirely.
        fn ensure_capacity(
            &mut self,
            shm: &wl_shm::WlShm,
            qh: &QueueHandle<WlState>,
            needed_slot_bytes: usize,
        ) -> bool {
            let total = needed_slot_bytes * 2;
            if total <= self.capacity {
                return true;
            }
            for slot in &mut self.buffers {
                if let Some(buf) = slot.take() {
                    buf.destroy();
                }
            }
            self.pool.destroy();
            // SAFETY: ptr and capacity came from a successful mmap.
            let _ = unsafe { rustix::mm::munmap(self.ptr, self.capacity) };

            let fd = match memfd_create("buffr-statusline-shm", MemfdFlags::CLOEXEC) {
                Ok(f) => f,
                Err(e) => {
                    warn!(?e, "memfd_create (resize) failed");
                    return false;
                }
            };
            if let Err(e) = ftruncate(&fd, total as u64) {
                warn!(?e, "ftruncate (resize) failed");
                return false;
            }
            let ptr = match unsafe {
                mmap(
                    std::ptr::null_mut(),
                    total,
                    ProtFlags::READ | ProtFlags::WRITE,
                    MapFlags::SHARED,
                    &fd,
                    0,
                )
            } {
                Ok(p) => p,
                Err(e) => {
                    warn!(?e, "mmap (resize) failed");
                    return false;
                }
            };
            use std::os::unix::io::AsFd;
            let pool = shm.create_pool(fd.as_fd(), total as i32, qh, ());
            debug!(bytes = total, "shm pool recreated (capacity grow)");
            self.fd = fd;
            self.ptr = ptr;
            self.capacity = total;
            self.pool = pool;
            self.width = 0;
            true
        }

        /// Return a mutable slice over slot `slot`'s pixel data for `width`.
        ///
        /// # Safety
        ///
        /// Caller must ensure `(slot+1) * width * STATUSLINE_HEIGHT * 4 <= capacity`.
        unsafe fn pixels_mut_unchecked(&mut self, slot: usize, width: u32) -> &mut [u32] {
            let count = (width * STATUSLINE_HEIGHT) as usize;
            let slot_offset = slot * count;
            // SAFETY: pool is sized for 2 slots of width × STATUSLINE_HEIGHT pixels each.
            unsafe {
                let base = (self.ptr as *mut u32).add(slot_offset);
                std::slice::from_raw_parts_mut(base, count)
            }
        }

        /// Create (or replace) both slot wl_buffers for the new width.
        fn create_buffers(&mut self, qh: &QueueHandle<WlState>, width: u32) {
            for slot in &mut self.buffers {
                if let Some(buf) = slot.take() {
                    buf.destroy();
                }
            }
            let stride = (width * 4) as i32;
            let slot_bytes = Self::slot_bytes(width) as i32;
            for (i, slot) in self.buffers.iter_mut().enumerate() {
                let buf = self.pool.create_buffer(
                    i as i32 * slot_bytes,
                    width as i32,
                    STATUSLINE_HEIGHT as i32,
                    stride,
                    wl_shm::Format::Argb8888,
                    qh,
                    (),
                );
                *slot = Some(buf);
            }
            self.width = width;
            self.next_slot = 0;
        }
    }

    // ---- Public type exposed to main.rs ----------------------------------

    pub struct WaylandSub {
        connection: Connection,
        event_queue: EventQueue<WlState>,
        state: WlState,
        qh: QueueHandle<WlState>,
        _globals: GlobalList,
        child_surface: wl_surface::WlSurface,
        child_subsurface: wl_subsurface::WlSubsurface,
        shm_pool: ShmPool,
        /// Pixel width of the subsurface (= window width).
        current_w: u32,
        /// Pixel height of the window (used to position subsurface).
        current_h: u32,
    }

    impl WaylandSub {
        /// Try to set up a `wl_subsurface` for the statusline.
        ///
        /// Returns `None` when:
        /// - the window is not Wayland (X11, macOS, etc.)
        /// - a required global is missing
        /// - any allocation step fails
        pub fn new(window: &winit::window::Window) -> Option<Self> {
            use raw_window_handle::{
                HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
            };

            // ---- Extract Wayland handles from winit ----------------------
            let display_handle = window.display_handle().ok()?;
            let window_handle = window.window_handle().ok()?;

            // wl_display pointer — `NonNull<c_void>` from raw-window-handle.
            let wl_display_nonnull = match display_handle.as_raw() {
                RawDisplayHandle::Wayland(h) => h.display,
                _ => {
                    info!("wayland_sub: not a Wayland display — skipping subsurface init");
                    return None;
                }
            };

            // wl_surface pointer (parent toplevel surface from winit).
            let wl_surface_nonnull = match window_handle.as_raw() {
                RawWindowHandle::Wayland(h) => h.surface,
                _ => {
                    info!("wayland_sub: not a Wayland window handle — skipping");
                    return None;
                }
            };

            // ---- Build a Connection from the foreign display ptr ---------
            //
            // SAFETY: winit owns the display for the lifetime of the window,
            // which outlives WaylandSub. The NonNull ptr is valid and won't
            // be moved or freed while the window is alive.
            let backend = unsafe {
                Backend::from_foreign_display(
                    wl_display_nonnull.as_ptr() as *mut wayland_sys::client::wl_display
                )
            };
            let connection = Connection::from_backend(backend);

            // ---- Bind globals via a one-shot roundtrip -------------------
            let mut state = WlState::new();
            let (globals, mut event_queue) = match registry_queue_init::<WlState>(&connection) {
                Ok(pair) => pair,
                Err(e) => {
                    warn!(?e, "wayland_sub: registry_queue_init failed");
                    return None;
                }
            };
            let qh = event_queue.handle();

            let compositor: wl_compositor::WlCompositor = match globals.bind(&qh, 4..=6, ()) {
                Ok(c) => c,
                Err(e) => {
                    warn!(?e, "wayland_sub: wl_compositor not available (need v4+)");
                    return None;
                }
            };
            let subcompositor: wl_subcompositor::WlSubcompositor =
                match globals.bind(&qh, 1..=1, ()) {
                    Ok(sc) => sc,
                    Err(e) => {
                        warn!(?e, "wayland_sub: wl_subcompositor not available");
                        return None;
                    }
                };
            let shm: wl_shm::WlShm = match globals.bind(&qh, 1..=2, ()) {
                Ok(s) => s,
                Err(e) => {
                    warn!(?e, "wayland_sub: wl_shm not available");
                    return None;
                }
            };

            if let Err(e) = event_queue.roundtrip(&mut state) {
                warn!(?e, "wayland_sub: roundtrip failed");
                return None;
            }

            info!(
                compositor_ver = compositor.version(),
                subcompositor_ver = subcompositor.version(),
                shm_ver = shm.version(),
                "wayland_sub: globals bound",
            );

            // ---- Wrap the parent wl_surface from winit's raw pointer -----
            //
            // SAFETY: wl_surface_nonnull is a valid wl_surface proxy owned by
            // winit for the window's lifetime.
            let parent_obj_id = match unsafe {
                wayland_client::backend::ObjectId::from_ptr(
                    wl_surface::WlSurface::interface(),
                    wl_surface_nonnull.as_ptr() as *mut wayland_sys::client::wl_proxy,
                )
            } {
                Ok(id) => id,
                Err(e) => {
                    warn!(?e, "wayland_sub: from_ptr for parent surface failed");
                    return None;
                }
            };

            let parent_surface = match wl_surface::WlSurface::from_id(&connection, parent_obj_id) {
                Ok(s) => s,
                Err(e) => {
                    warn!(?e, "wayland_sub: WlSurface::from_id failed");
                    return None;
                }
            };

            info!(id = %parent_surface.id(), "wayland_sub: parent surface attached");

            // ---- Create child surface + subsurface -----------------------
            let child_surface = compositor.create_surface(&qh, ());
            let child_subsurface =
                subcompositor.get_subsurface(&child_surface, &parent_surface, &qh, ());

            // Sync mode (the wl_subsurface default — we omit set_desync).
            // In sync mode the child surface's pending state (new buffer,
            // damage) and the subsurface's own pending state (set_position)
            // are both applied atomically on the PARENT's commit. Under
            // rapid top-edge drag desync mode let the compositor see a
            // frame where the new buffer was applied but the new position
            // wasn't yet (or the inverse), making the statusline briefly
            // jump. Sync mode collapses both updates into the same parent
            // commit boundary so the user only ever observes consistent
            // states.
            //
            // Tradeoff: chrome state changes that don't resize the window
            // (URL change, mode change) still need a parent commit to be
            // visible. paint_chrome already commits the parent via wgpu
            // present whenever chrome_generation bumps, so this is free.

            // Crucially, leak the parent_surface wrapper. We constructed it
            // from a raw ptr that winit owns; dropping our wrapper would
            // send a wl_surface.destroy for winit's surface and double-free
            // it when winit later tears down the toplevel. winit retains
            // its own proxy and is the canonical owner.
            std::mem::forget(parent_surface);

            // ---- Allocate initial shm pool (4096 px wide × SL_HEIGHT, 2 slots) ---
            let initial_slot_bytes = ShmPool::slot_bytes(4096);
            let mut shm_pool = ShmPool::new(&shm, &qh, initial_slot_bytes)?;

            // Create the initial buffers (assume 1280 px wide until first resize).
            let init_w: u32 = 1280;
            shm_pool.create_buffers(&qh, init_w);

            state.compositor = Some(compositor);
            state.subcompositor = Some(subcompositor);
            state.shm = Some(shm);

            Some(Self {
                connection,
                event_queue,
                state,
                qh,
                _globals: globals,
                child_surface,
                child_subsurface,
                shm_pool,
                current_w: init_w,
                current_h: 800,
            })
        }

        /// Update the subsurface position for a new window size.
        ///
        /// Grows the shm pool if needed. Does **not** commit — position update
        /// becomes visible on the next `paint()` commit.
        pub fn set_size(&mut self, window_w: u32, window_h: u32) {
            self.current_w = window_w.max(1);
            self.current_h = window_h.max(1);
            let sl_y = (window_h as i32) - (STATUSLINE_HEIGHT as i32);
            let needed_slot_bytes = ShmPool::slot_bytes(self.current_w);

            let shm = match self.state.shm.as_ref() {
                Some(s) => s.clone(),
                None => return,
            };
            if !self
                .shm_pool
                .ensure_capacity(&shm, &self.qh, needed_slot_bytes)
            {
                return;
            }
            // Buffer width changed — recreate both slot buffers with new stride.
            if self.shm_pool.width != self.current_w {
                self.shm_pool.create_buffers(&self.qh, self.current_w);
            }

            self.child_subsurface.set_position(0, sl_y.max(0));

            // Flush so the set_position protocol message reaches libwayland's
            // send queue before wgpu's surface present writes the parent
            // commit. Otherwise wayland-client may keep the message buffered
            // and the parent commit lands without the new pending position
            // applied — same observable result as setting position after
            // commit (subsurface tracks the previous resize).
            if let Err(e) = self.connection.flush() {
                debug!(?e, "wayland_sub: flush after set_position warning");
            }

            debug!(window_w, window_h, sl_y, "wayland_sub: set_size");
        }

        /// Paint the subsurface by invoking `paint_fn` with a mmap'd pixel slice.
        ///
        /// The slice is `width * STATUSLINE_HEIGHT` u32 pixels, row-major BGRA.
        /// After the callback returns, the buffer is attached, damaged, and
        /// committed on the child surface.
        pub fn paint<F: FnOnce(&mut [u32], usize, usize)>(&mut self, paint_fn: F) {
            let w = self.current_w;
            let h = STATUSLINE_HEIGHT;
            if w == 0 {
                return;
            }
            let needed_slot_bytes = ShmPool::slot_bytes(w);
            let shm = match self.state.shm.as_ref() {
                Some(s) => s.clone(),
                None => return,
            };
            if !self
                .shm_pool
                .ensure_capacity(&shm, &self.qh, needed_slot_bytes)
            {
                return;
            }
            if self.shm_pool.width != w {
                self.shm_pool.create_buffers(&self.qh, w);
            }

            // Pick the next slot — alternate every paint so the compositor
            // never sees us writing into a buffer it might still be reading
            // from a recent display cycle. With single-buffer reuse during
            // rapid resize, the compositor occasionally grabbed a frame
            // mid-paint and showed a torn statusline before the next commit
            // corrected it.
            let slot = self.shm_pool.next_slot;
            self.shm_pool.next_slot = 1 - slot;

            // Call the painter into this slot's mmap'd pixel slice.
            {
                // SAFETY: 2 × w × STATUSLINE_HEIGHT × 4 <= capacity (ensured above).
                let pixels = unsafe { self.shm_pool.pixels_mut_unchecked(slot, w) };
                paint_fn(pixels, w as usize, h as usize);
            }

            // Attach this slot's buffer + damage + commit on the child.
            if let Some(buf) = self.shm_pool.buffers[slot].as_ref() {
                self.child_surface.attach(Some(buf), 0, 0);
                self.child_surface.damage_buffer(0, 0, w as i32, h as i32);
                self.child_surface.commit();
            }

            // Dispatch our event queue to flush all pending writes.
            if let Err(e) = self.event_queue.flush() {
                debug!(?e, "wayland_sub: paint flush warning");
            }
        }

        /// Flush the Wayland connection. Call once per `about_to_wait` tick.
        pub fn flush(&mut self) {
            if let Err(e) = self.connection.flush() {
                debug!(?e, "wayland_sub: flush warning");
            }
        }
    }
}

// Re-export the real type on Linux.
#[cfg(target_os = "linux")]
pub use linux::WaylandSub;
