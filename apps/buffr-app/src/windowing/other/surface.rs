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
}
