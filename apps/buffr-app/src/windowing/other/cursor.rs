//! Cursor shape enum. Verbatim copy of wayr's `CursorIcon` plus a
//! mapping into `winit::window::CursorIcon` so `Window::set_cursor`
//! works on macOS / Windows.

/// Logical cursor shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum CursorIcon {
    /// Standard arrow.
    #[default]
    Default,
    /// Context menu available.
    ContextMenu,
    /// Help / question mark.
    Help,
    /// Pointing hand (link / clickable).
    Pointer,
    /// Progress indicator (busy but still interactive).
    Progress,
    /// Loading / busy spinner (blocked).
    Wait,
    /// A cell or set of cells may be selected.
    Cell,
    /// Crosshair (precision selection).
    Crosshair,
    /// I-beam (text editing).
    Text,
    /// Vertical text I-beam.
    VerticalText,
    /// Drag-and-drop: alias of / shortcut to something.
    Alias,
    /// Drag-and-drop: copy.
    Copy,
    /// Move / drag.
    Move,
    /// Drag-and-drop: cannot be dropped here.
    NoDrop,
    /// Drag-and-drop: forbidden action.
    NotAllowed,
    /// Drag-and-drop: something can be grabbed.
    Grab,
    /// Drag-and-drop: something is being grabbed.
    Grabbing,
    /// Resize: east border.
    EResize,
    /// Resize: north border.
    NResize,
    /// Resize: north-east corner.
    NeResize,
    /// Resize: north-west corner.
    NwResize,
    /// Resize: south border.
    SResize,
    /// Resize: south-east corner.
    SeResize,
    /// Resize: south-west corner.
    SwResize,
    /// Resize: west border.
    WResize,
    /// Resize: east-west.
    EwResize,
    /// Resize: north-south.
    NsResize,
    /// Resize: north-east-south-west diagonal.
    NeswResize,
    /// Resize: north-west-south-east diagonal.
    NwseResize,
    /// Resize: column (horizontal).
    ColResize,
    /// Resize: row (vertical).
    RowResize,
    /// Scrollable in any direction.
    AllScroll,
    /// Zoom in.
    ZoomIn,
    /// Zoom out.
    ZoomOut,
}

impl CursorIcon {
    /// Map to winit's `CursorIcon`.
    pub(super) fn to_winit(self) -> winit::window::CursorIcon {
        use winit::window::CursorIcon as W;
        match self {
            CursorIcon::Default => W::Default,
            CursorIcon::ContextMenu => W::ContextMenu,
            CursorIcon::Help => W::Help,
            CursorIcon::Pointer => W::Pointer,
            CursorIcon::Progress => W::Progress,
            CursorIcon::Wait => W::Wait,
            CursorIcon::Cell => W::Cell,
            CursorIcon::Crosshair => W::Crosshair,
            CursorIcon::Text => W::Text,
            CursorIcon::VerticalText => W::VerticalText,
            CursorIcon::Alias => W::Alias,
            CursorIcon::Copy => W::Copy,
            CursorIcon::Move => W::Move,
            CursorIcon::NoDrop => W::NoDrop,
            CursorIcon::NotAllowed => W::NotAllowed,
            CursorIcon::Grab => W::Grab,
            CursorIcon::Grabbing => W::Grabbing,
            CursorIcon::EResize => W::EResize,
            CursorIcon::NResize => W::NResize,
            CursorIcon::NeResize => W::NeResize,
            CursorIcon::NwResize => W::NwResize,
            CursorIcon::SResize => W::SResize,
            CursorIcon::SeResize => W::SeResize,
            CursorIcon::SwResize => W::SwResize,
            CursorIcon::WResize => W::WResize,
            CursorIcon::EwResize => W::EwResize,
            CursorIcon::NsResize => W::NsResize,
            CursorIcon::NeswResize => W::NeswResize,
            CursorIcon::NwseResize => W::NwseResize,
            CursorIcon::ColResize => W::ColResize,
            CursorIcon::RowResize => W::RowResize,
            CursorIcon::AllScroll => W::AllScroll,
            CursorIcon::ZoomIn => W::ZoomIn,
            CursorIcon::ZoomOut => W::ZoomOut,
        }
    }
}
