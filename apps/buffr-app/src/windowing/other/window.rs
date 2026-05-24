//! `Window` wrapper around `winit::window::Window`. Exposes the
//! wayr-shaped `Toplevel` API (renamed `Window` here for cross-
//! platform symmetry with the Linux backend, which `use wayr::Toplevel
//! as Window`).
//!
//! Construction goes via [`ToplevelBuilder`] — the builder collects
//! initial attributes and hands them to `EventLoop::build_window` for
//! actual creation (winit only lets us call `create_window` while a
//! borrow on `ActiveEventLoop` is live).

use std::sync::Arc;

use super::cursor::CursorIcon;
use super::event_loop::EventLoop;
use super::geometry::Size;
use super::surface::{RawWindowHandlePlaceholder, Surface, SurfaceId};

/// A regular top-level window. Wraps `winit::window::Window`.
pub struct Window {
    pub(super) id: SurfaceId,
    pub(super) inner: Arc<winit::window::Window>,
}

impl Window {
    /// Start building a new top-level window.
    pub fn builder() -> ToplevelBuilder {
        ToplevelBuilder::default()
    }

    /// Set the window title.
    pub fn set_title(&self, title: impl Into<String>) {
        self.inner.set_title(&title.into());
    }

    /// Set the minimum logical size.
    pub fn set_min_size(&self, size: Option<Size>) {
        match size {
            Some(s) => self
                .inner
                .set_min_inner_size(Some(winit::dpi::LogicalSize::new(s.width, s.height))),
            None => self
                .inner
                .set_min_inner_size(None::<winit::dpi::LogicalSize<u32>>),
        }
    }

    /// Set the maximum logical size.
    pub fn set_max_size(&self, size: Option<Size>) {
        match size {
            Some(s) => self
                .inner
                .set_max_inner_size(Some(winit::dpi::LogicalSize::new(s.width, s.height))),
            None => self
                .inner
                .set_max_inner_size(None::<winit::dpi::LogicalSize<u32>>),
        }
    }

    /// Whether the OS has marked this window as obscured. winit
    /// doesn't expose a synchronous query for this on macOS / Windows,
    /// so this always returns `false` — the `WindowEvent::Occluded`
    /// path remains the source of truth.
    pub fn is_occluded(&self) -> bool {
        false
    }

    /// Programmatically request the OS close this window.
    /// winit has no direct equivalent; the consumer typically drops
    /// the `Window` after receiving `CloseRequested`. This method
    /// exists for API symmetry with the Linux backend.
    pub fn request_close(&self) {
        // No-op on winit. Consumers detect the close via
        // `WindowEvent::CloseRequested`.
    }

    /// Linux-only API — returns `None` on macOS / Windows. Kept for
    /// signature parity; the Wayland-native handle extraction path
    /// in `main.rs` is `#[cfg(target_os = "linux")]`-gated.
    pub fn wl_surface_ptr(&self) -> Option<std::ptr::NonNull<std::ffi::c_void>> {
        None
    }

    /// Physical buffer size in pixels.
    pub fn physical_size(&self) -> Size {
        let s = self.inner.inner_size();
        Size::new(s.width, s.height)
    }

    /// Effective scale factor.
    pub fn scale_factor(&self) -> f64 {
        self.inner.scale_factor()
    }

    /// Set the cursor shape shown over this window.
    pub fn set_cursor<T>(&self, _event_loop: &EventLoop<T>, icon: CursorIcon) {
        self.inner.set_cursor(icon.to_winit());
    }

    /// IME accessor. winit does not expose a per-surface IME handle
    /// on macOS / Windows the way wayr does (text-input-v3 is a
    /// Wayland-only protocol). Returns `None`; the consumer should
    /// treat IME as unsupported on this platform.
    ///
    /// TODO(windowing): wire `winit::Window::set_ime_allowed` +
    /// `set_ime_cursor_area` once the buffr IME story needs the
    /// platform-IME path.
    pub fn ime<T>(&self, _event_loop: &EventLoop<T>) -> Option<()> {
        None
    }

    /// Request the OS focus this window. Maps to
    /// `winit::Window::focus_window`. The OS may reject focus-steal
    /// attempts depending on platform policy.
    pub fn request_activation<T>(
        &self,
        _event_loop: &mut EventLoop<T>,
    ) -> Result<(), ActivationError> {
        self.inner.focus_window();
        Ok(())
    }

    /// Bypass the activation-token handshake — winit always treats
    /// `focus_window` as best-effort, and no activation tokens are
    /// involved on macOS / Windows.
    pub fn set_activation_token<T>(
        &self,
        _event_loop: &EventLoop<T>,
        _token: impl Into<String>,
    ) -> Result<(), ActivationError> {
        Ok(())
    }

    /// Request the OS schedule a redraw.
    pub fn request_redraw(&self) {
        self.inner.request_redraw();
    }
}

impl Surface for Window {
    fn id(&self) -> SurfaceId {
        self.id
    }

    fn size(&self) -> Size {
        // Logical surface size — divide physical by scale.
        let phys = self.inner.inner_size();
        let scale = self.inner.scale_factor();
        let lw = ((phys.width as f64) / scale).round() as u32;
        let lh = ((phys.height as f64) / scale).round() as u32;
        Size::new(lw.max(1), lh.max(1))
    }

    fn scale_factor(&self) -> f64 {
        self.inner.scale_factor()
    }

    fn request_redraw(&self) {
        self.inner.request_redraw();
    }

    fn raw_window_handle(&self) -> RawWindowHandlePlaceholder {
        // No native handle exposed through this path — consumers use
        // the `raw_window_handle` 0.6 traits directly on
        // `Arc<Window>` via the `HasWindowHandle` impl below.
        // Returning a dangling NonNull would be unsound; instead we
        // hand back a non-null placeholder pointing at our inner
        // Arc, which the consumer must NOT dereference. This method
        // exists only for API parity with the Linux backend; nothing
        // in buffr-app calls it on non-Linux.
        let ptr = (Arc::as_ptr(&self.inner) as *const std::ffi::c_void) as *mut std::ffi::c_void;
        let nn = std::ptr::NonNull::new(ptr).expect("Arc::as_ptr never null");
        RawWindowHandlePlaceholder { wl_surface: nn }
    }
}

// raw-window-handle 0.6: delegate to the inner winit Window which
// already implements both traits.
impl raw_window_handle::HasWindowHandle for Window {
    fn window_handle(
        &self,
    ) -> std::result::Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError>
    {
        self.inner.window_handle()
    }
}

impl raw_window_handle::HasDisplayHandle for Window {
    fn display_handle(
        &self,
    ) -> std::result::Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError>
    {
        self.inner.display_handle()
    }
}

// Compile-time guarantee: `Arc<Window>` is `Send + Sync + 'static`
// (matches the wgpu `SurfaceTarget::Window` bound used by
// `Renderer::new`).
const _: fn() = || {
    fn assert_send_sync_static<T: Send + Sync + 'static>() {}
    assert_send_sync_static::<Window>();
};

/// Builder for [`Window`]. Mirrors wayr's `ToplevelBuilder` so the
/// call sites in main.rs (`Toplevel::builder().with_title(...)...`)
/// compile unchanged.
#[derive(Debug, Default)]
pub struct ToplevelBuilder {
    pub(super) title: Option<String>,
    pub(super) app_id: Option<String>,
    pub(super) initial_size: Option<Size>,
    pub(super) min_size: Option<Size>,
    pub(super) max_size: Option<Size>,
}

impl ToplevelBuilder {
    /// Set the window title.
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Set the application id (Wayland `set_app_id` / equivalent).
    /// Not natively used by winit, but kept for parity.
    pub fn with_app_id(mut self, app_id: impl Into<String>) -> Self {
        self.app_id = Some(app_id.into());
        self
    }

    /// Set the initial logical surface size.
    pub fn with_initial_size(mut self, size: Size) -> Self {
        self.initial_size = Some(size);
        self
    }

    /// Set the minimum logical size.
    pub fn with_min_size(mut self, size: Size) -> Self {
        self.min_size = Some(size);
        self
    }

    /// Set the maximum logical size.
    pub fn with_max_size(mut self, size: Size) -> Self {
        self.max_size = Some(size);
        self
    }

    /// Construct the top-level window.
    pub fn build<T>(self, event_loop: &mut EventLoop<T>) -> Result<Window, BuildError> {
        event_loop.build_window(self)
    }
}

/// Error type for window construction.
///
/// Kept as a simple wrapper around winit's `OsError` (formatted as a
/// string so we don't need to re-export the entire winit error
/// hierarchy through the bridge).
#[derive(Debug)]
pub struct BuildError(pub String);

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BuildError {}

/// Error returned by [`Window::request_activation`] /
/// [`Window::set_activation_token`]. Kept for shape-parity with wayr;
/// winit's `focus_window` never reports failure, so this is
/// effectively dead on macOS / Windows.
#[derive(Debug)]
#[non_exhaustive]
pub enum ActivationError {
    /// Platform does not implement programmatic activation.
    Unsupported,
    /// No recent user input to attach as the activation serial.
    NoInputSerial,
}

impl std::fmt::Display for ActivationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ActivationError::Unsupported => f.write_str("activation not supported"),
            ActivationError::NoInputSerial => f.write_str("no input serial available"),
        }
    }
}

impl std::error::Error for ActivationError {}
