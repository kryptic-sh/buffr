//! IME types. Verbatim copy of wayr's IME shape so the buffr-app
//! `WindowEvent::Ime` match arm in main.rs continues to work.
//!
//! winit's IME story is platform-specific and not used by buffr-app
//! on macOS / Windows in v0.1, so the bridge emitter (in event.rs)
//! maps winit's `Ime::Preedit` / `Ime::Commit` events through to the
//! shape below. winit's `Enabled` / `Disabled` lifecycle variants are
//! discarded (wayr doesn't emit them — IME enable is consumer-driven
//! on the Linux backend).

/// IME event dispatched as part of [`crate::windowing::WindowEvent::Ime`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ImeEvent {
    /// Composition string visible to the user but not yet committed.
    Preedit {
        /// The current composition string.
        text: String,
        /// Caret byte offset inside `text`, or `None` to hide caret.
        cursor: Option<u32>,
    },
    /// Final committed string.
    Commit(String),
    /// IME wants the consumer to delete `before_bytes` UTF-8 bytes
    /// before the cursor + `after_bytes` after, then commit.
    DeleteSurroundingText {
        /// Bytes to delete before the cursor.
        before_bytes: u32,
        /// Bytes to delete after the cursor.
        after_bytes: u32,
    },
}
