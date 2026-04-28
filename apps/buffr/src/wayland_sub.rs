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

    // ---- Shm pool + mapped buffer ----------------------------------------

    struct ShmPool {
        /// The anonymous file backing the shm pool.
        fd: OwnedFd,
        /// Mapped memory as raw pointer. Length = capacity (bytes).
        ptr: *mut c_void,
        /// Capacity in bytes (allocated).
        capacity: usize,
        /// wl_shm_pool handle.
        pool: wl_shm_pool::WlShmPool,
        /// Current width (px) of the buffer.
        width: u32,
        /// wl_buffer wrapping the pool bytes.
        buffer: Option<wl_buffer::WlBuffer>,
    }

    // SAFETY: the mmap'd pointer is only touched from the main thread.
    unsafe impl Send for ShmPool {}

    impl ShmPool {
        /// Create a new shm pool big enough for `initial_bytes`.
        fn new(
            shm: &wl_shm::WlShm,
            qh: &QueueHandle<WlState>,
            initial_bytes: usize,
        ) -> Option<Self> {
            let fd = memfd_create("buffr-statusline-shm", MemfdFlags::CLOEXEC)
                .map_err(|e| warn!(?e, "memfd_create failed"))
                .ok()?;
            ftruncate(&fd, initial_bytes as u64)
                .map_err(|e| warn!(?e, "ftruncate failed"))
                .ok()?;
            // SAFETY: fd is a valid memfd sized to initial_bytes.
            let ptr = unsafe {
                mmap(
                    std::ptr::null_mut(),
                    initial_bytes,
                    ProtFlags::READ | ProtFlags::WRITE,
                    MapFlags::SHARED,
                    &fd,
                    0,
                )
            }
            .map_err(|e| warn!(?e, "mmap failed"))
            .ok()?;

            use std::os::unix::io::AsFd;
            let pool = shm.create_pool(fd.as_fd(), initial_bytes as i32, qh, ());
            debug!(bytes = initial_bytes, "shm pool created");

            Some(Self {
                fd,
                ptr,
                capacity: initial_bytes,
                pool,
                width: 0,
                buffer: None,
            })
        }

        /// Grow the pool if `needed_bytes` exceeds the current capacity.
        /// Recreates the pool entirely (correct for PoC; no double-buffering).
        fn ensure_capacity(
            &mut self,
            shm: &wl_shm::WlShm,
            qh: &QueueHandle<WlState>,
            needed_bytes: usize,
        ) -> bool {
            if needed_bytes <= self.capacity {
                return true;
            }
            // Destroy old buffer + pool, unmap, realloc.
            if let Some(buf) = self.buffer.take() {
                buf.destroy();
            }
            self.pool.destroy();
            // Unmap old memory.
            // SAFETY: ptr and capacity came from a successful mmap.
            let _ = unsafe { rustix::mm::munmap(self.ptr, self.capacity) };

            let fd = match memfd_create("buffr-statusline-shm", MemfdFlags::CLOEXEC) {
                Ok(f) => f,
                Err(e) => {
                    warn!(?e, "memfd_create (resize) failed");
                    return false;
                }
            };
            if let Err(e) = ftruncate(&fd, needed_bytes as u64) {
                warn!(?e, "ftruncate (resize) failed");
                return false;
            }
            let ptr = match unsafe {
                mmap(
                    std::ptr::null_mut(),
                    needed_bytes,
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
            let pool = shm.create_pool(fd.as_fd(), needed_bytes as i32, qh, ());
            debug!(bytes = needed_bytes, "shm pool recreated (capacity grow)");
            self.fd = fd;
            self.ptr = ptr;
            self.capacity = needed_bytes;
            self.pool = pool;
            true
        }

        /// Return a mutable slice over the pixel data for `width × STATUSLINE_HEIGHT`.
        ///
        /// # Safety
        ///
        /// Caller must ensure `width * STATUSLINE_HEIGHT * 4 <= capacity`.
        unsafe fn pixels_mut_unchecked(&mut self, width: u32) -> &mut [u32] {
            let count = (width * STATUSLINE_HEIGHT) as usize;
            // SAFETY: ptr is mapped RW, count*4 <= capacity, only accessed here.
            unsafe { std::slice::from_raw_parts_mut(self.ptr as *mut u32, count) }
        }

        /// Create (or replace) the wl_buffer for the current width.
        fn create_buffer(&mut self, qh: &QueueHandle<WlState>, width: u32) {
            if let Some(old) = self.buffer.take() {
                old.destroy();
            }
            let buf = self.pool.create_buffer(
                0,
                width as i32,
                STATUSLINE_HEIGHT as i32,
                (width * 4) as i32,
                wl_shm::Format::Argb8888,
                qh,
                (),
            );
            self.buffer = Some(buf);
            self.width = width;
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

            // Desync mode: child commits are independent of the parent.
            child_subsurface.set_desync();

            // ---- Allocate initial shm pool (4096 px wide × SL_HEIGHT) ---
            let initial_bytes = (4096 * STATUSLINE_HEIGHT * 4) as usize;
            let mut shm_pool = ShmPool::new(&shm, &qh, initial_bytes)?;

            // Create the initial buffer (assume 1280 px wide until first resize).
            let init_w: u32 = 1280;
            shm_pool.create_buffer(&qh, init_w);

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
            let needed_bytes = (self.current_w * STATUSLINE_HEIGHT * 4) as usize;

            let shm = match self.state.shm.as_ref() {
                Some(s) => s.clone(),
                None => return,
            };
            if !self.shm_pool.ensure_capacity(&shm, &self.qh, needed_bytes) {
                return;
            }
            // Buffer width changed — recreate buffer with new stride.
            if self.shm_pool.width != self.current_w {
                self.shm_pool.create_buffer(&self.qh, self.current_w);
            }

            self.child_subsurface.set_position(0, sl_y.max(0));
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
            let needed = (w * h * 4) as usize;
            let shm = match self.state.shm.as_ref() {
                Some(s) => s.clone(),
                None => return,
            };
            if !self.shm_pool.ensure_capacity(&shm, &self.qh, needed) {
                return;
            }
            if self.shm_pool.width != w {
                self.shm_pool.create_buffer(&self.qh, w);
            }

            // Call the painter into the mmap'd pixel slice.
            {
                // SAFETY: w * STATUSLINE_HEIGHT * 4 <= capacity (ensured above).
                let pixels = unsafe { self.shm_pool.pixels_mut_unchecked(w) };
                paint_fn(pixels, w as usize, h as usize);
            }

            // Attach + damage + commit the child surface.
            if let Some(buf) = &self.shm_pool.buffer {
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
