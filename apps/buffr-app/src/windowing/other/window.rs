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
use super::surface::{Surface, SurfaceId};

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

/// Error returned by [`Window::request_activation`]. Kept for
/// shape-parity with wayr; winit's `focus_window` never reports
/// failure, so neither variant is constructed on macOS / Windows.
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
