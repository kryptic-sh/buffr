//! Surface trait + `SurfaceId`. Matches wayr's shape.
//!
//! `SurfaceId` wraps `NonZeroU64` so it stays compact in
//! `Option<SurfaceId>` slots — the bridge maps from winit's
//! `WindowId` (which is a `u64` newtype with no nonzero guarantee)
//! by allocating monotonic ids out of the [`EventLoop`]'s internal
//! counter.

use std::num::NonZeroU64;

use super::geometry::Size;

/// Identifier unique per surface within a single
/// [`crate::windowing::EventLoop`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SurfaceId(NonZeroU64);

impl SurfaceId {
    /// Construct a `SurfaceId` from a raw `u64`. `0` is reserved and
    /// returns `None`.
    pub fn from_raw(raw: u64) -> Option<Self> {
        NonZeroU64::new(raw).map(Self)
    }

    /// Extract the raw `u64`.
    pub fn as_u64(self) -> u64 {
        self.0.get()
    }

    /// Internal constructor — used by the event-loop dispatch path
    /// when allocating ids for newly-created winit windows.
    pub(super) fn from_nonzero(n: NonZeroU64) -> Self {
        Self(n)
    }
}

/// Shared interface every surface kind implements.
pub trait Surface {
    /// Stable identifier for matching event-loop events back to this
    /// surface.
    fn id(&self) -> SurfaceId;

    /// Current logical surface size in scale-adjusted pixels.
    fn size(&self) -> Size;

    /// Current effective scale factor.
    fn scale_factor(&self) -> f64;

    /// Request the compositor schedule a redraw.
    fn request_redraw(&self);

    /// Raw window handle placeholder. The concrete
    /// `raw-window-handle 0.6` traits are implemented directly on
    /// the [`super::Window`] type — consumers usually go through
    /// those traits instead of this method.
    fn raw_window_handle(&self) -> RawWindowHandlePlaceholder;
}

/// Placeholder kept for API parity with wayr; the real
/// `raw-window-handle 0.6` impls live on [`super::Window`].
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct RawWindowHandlePlaceholder {
    /// Opaque platform-specific pointer.
    pub wl_surface: std::ptr::NonNull<std::ffi::c_void>,
}

// SAFETY: opaque pointer; carrying across threads is caller's
// responsibility (matches wayr's safety statement).
unsafe impl Send for RawWindowHandlePlaceholder {}
unsafe impl Sync for RawWindowHandlePlaceholder {}
