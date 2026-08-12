//! Output (monitor) info. Verbatim copy of wayr's `OutputInfo`
//! shape — the buffr-app event loop uses [`OutputInfo::refresh_mhz`]
//! to pace wakeups to the display's refresh rate.
//!
//! On the non-Linux backend the [`super::EventLoop::outputs`] accessor
//! enumerates winit's `available_monitors()` and converts each
//! `MonitorHandle` into one of these snapshots.

use super::geometry::Size;

/// Stable per-output identifier.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct OutputId(pub(crate) u64);

/// Snapshot of a monitor's state.
#[derive(Debug, Clone, Default)]
pub struct OutputInfo {
    /// Stable id assigned at enumeration time.
    pub id: OutputId,
    /// Compositor's machine-readable name (e.g. `"DP-1"`, `"HDMI-A-1"`).
    pub name: Option<String>,
    /// Integer scale (always at least 1).
    pub scale: i32,
    /// Physical size of the active mode in pixels.
    pub physical_size: Size,
    /// Position in compositor-global coordinates.
    pub position: (i32, i32),
    /// Refresh rate of the active mode, in millihertz.
    pub refresh_mhz: i32,
}
