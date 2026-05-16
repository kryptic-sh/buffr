//! [`BuffrBlitzShell`] — `ShellProvider` impl that captures cursor changes from
//! Blitz's push-model callback into a shared `Mutex` slot.
//!
//! Blitz calls `ShellProvider::set_cursor(CursorIcon)` on pointer-move whenever
//! the hovered element's computed `cursor` CSS property changes.  We capture
//! that notification and map the winit-style `CursorIcon` to a CEF cursor-type
//! `u32` so the rest of buffr can call `take_cursor_change()` and forward the
//! value to the host shell.
//!
//! # CEF cursor mapping
//!
//! | `CursorIcon`              | CEF u32 |
//! |---------------------------|---------|
//! | Default                   | 0       |
//! | Text / VerticalText       | 1       |
//! | Crosshair                 | 6       |
//! | Help                      | 8       |
//! | Move / AllScroll          | 9       |
//! | NotAllowed / NoDrop       | 12      |
//! | Wait                      | 13      |
//! | Pointer                   | 28      |
//! | Grab                      | 34      |
//! | Grabbing                  | 35      |
//! | *(any other)*             | 0       |

use std::sync::{Arc, Mutex};

use cursor_icon::CursorIcon;

use blitz_traits::shell::ShellProvider;

/// Maps a winit-style `CursorIcon` to a CEF cursor-type `u32`.
fn cursor_icon_to_cef(icon: CursorIcon) -> u32 {
    match icon {
        CursorIcon::Default => 0,
        CursorIcon::Text | CursorIcon::VerticalText => 1,
        CursorIcon::Crosshair => 6,
        CursorIcon::Help => 8,
        CursorIcon::Move | CursorIcon::AllScroll => 9,
        CursorIcon::NotAllowed | CursorIcon::NoDrop => 12,
        CursorIcon::Wait => 13,
        CursorIcon::Pointer => 28,
        CursorIcon::Grab => 34,
        CursorIcon::Grabbing => 35,
        _ => 0,
    }
}

/// `ShellProvider` that writes cursor changes into a shared slot.
///
/// Blitz calls `set_cursor` on each pointer-move when the computed cursor
/// changes.  We store `(browser_id=0, cef_kind)` into the Mutex; the engine
/// thread drains it via `take_cursor_change()`.
///
/// `browser_id` is always 0 — Blitz is a single-document renderer with no
/// per-browser tracking.
pub(crate) struct BuffrBlitzShell {
    pub cursor: Arc<Mutex<Option<(i32, u32)>>>,
}

impl ShellProvider for BuffrBlitzShell {
    fn set_cursor(&self, icon: CursorIcon) {
        let cef_kind = cursor_icon_to_cef(icon);
        tracing::debug!("blitz shell: set_cursor {:?} → CEF {}", icon, cef_kind);
        if let Ok(mut guard) = self.cursor.lock() {
            *guard = Some((0_i32, cef_kind));
        }
    }
}
