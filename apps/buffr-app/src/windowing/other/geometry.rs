//! Geometry primitives — verbatim copy of wayr's shape so the
//! buffr-app code uses one name across platforms.

/// Logical 2D position. Negative values are allowed (positioning a
/// subsurface above/left of its parent is valid).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Position {
    /// X coordinate, in logical pixels.
    pub x: i32,
    /// Y coordinate, in logical pixels.
    pub y: i32,
}

impl Position {
    /// Origin (0, 0).
    pub const ZERO: Self = Self { x: 0, y: 0 };

    /// Construct a new `Position`.
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

/// Logical 2D size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Size {
    /// Width, in logical pixels.
    pub width: u32,
    /// Height, in logical pixels.
    pub height: u32,
}

impl Size {
    /// Construct a new `Size`.
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }
}

/// Axis-aligned rectangle: a [`Position`] and a [`Size`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Rect {
    /// Top-left corner.
    pub position: Position,
    /// Width and height.
    pub size: Size,
}

impl Rect {
    /// Construct a new `Rect`.
    pub const fn new(position: Position, size: Size) -> Self {
        Self { position, size }
    }
}
