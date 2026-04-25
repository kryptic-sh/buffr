//! `BuffrHost` — the host adapter that wires `hjkl_engine::Editor` to
//! the buffr browser shell.
//!
//! Implements [`hjkl_engine::Host`] with `type Intent = BuffrEditIntent`.
//! Inherent helpers (`set_clipboard_cache`, `drain_clipboard_outbox`,
//! `drain_intents`) sit alongside the trait methods so the host's tick
//! loop can flush queued operations on its own cadence — the engine
//! never blocks on either clipboard or intent fan-out.

use hjkl_engine::{CursorShape, Host};
use std::time::Instant;

/// Buffer identifier in buffr's tab manager. Opaque — host owns the
/// generation; engine echoes it back in intents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BuffrBufferId(pub u64);

/// Intents the engine emits back at the host. Variants align with the
/// SPEC `Host::Intent` shape buffr will set when `hjkl_engine::Host`
/// ships.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuffrEditIntent {
    /// Form-field autocomplete trigger (`Ctrl-Space`, `<Tab>` in some
    /// configs). Host queries the page's form-fill or LSP-equivalent
    /// service.
    RequestAutocomplete,
    /// Switch focus to a different buffer (tab / textarea).
    SwitchBuffer(BuffrBufferId),
    /// User typed a key the page should see un-modified (e.g., `<Esc>`
    /// in a `contenteditable` should bubble to JS handlers).
    PassThrough,
}

/// Host adapter consumed by `hjkl_engine::Editor` once edit-mode is
/// active.
#[derive(Debug)]
pub struct BuffrHost {
    /// Last cursor shape requested by the engine. Drained by the host
    /// renderer per frame.
    pub last_cursor_shape: CursorShape,
    // Other fields intentionally below — keep `last_cursor_shape` first
    // so debug-printing the host shows the most recently observed
    // mode-derived state.
    /// Cached system clipboard value. Refreshed by the host on focus
    /// events / OSC52 reply / explicit poll. Reads from the engine
    /// return this slot directly — never block.
    clipboard_cache: Option<String>,
    /// Pending writes to the system clipboard. Flushed asynchronously
    /// by the host's tick loop; engine never awaits.
    clipboard_outbox: Vec<String>,
    /// Wall-clock start so timeouts can be expressed as `Duration` from
    /// editor construction time. Engine itself doesn't read clocks
    /// directly — it asks the host via `now()`.
    started: Instant,
    /// Intent queue drained by the host once per render frame.
    intents: Vec<BuffrEditIntent>,
}

impl Default for BuffrHost {
    fn default() -> Self {
        Self::new()
    }
}

impl BuffrHost {
    pub fn new() -> Self {
        Self {
            last_cursor_shape: CursorShape::Block,
            clipboard_cache: None,
            clipboard_outbox: Vec::new(),
            started: Instant::now(),
            intents: Vec::new(),
        }
    }

    /// Update the cached clipboard. Host calls this on focus events or
    /// when an OSC52 read reply arrives.
    pub fn set_clipboard_cache(&mut self, text: Option<String>) {
        self.clipboard_cache = text;
    }

    /// Drain pending clipboard writes. Host's tick loop calls this and
    /// dispatches each to the platform clipboard backend.
    pub fn drain_clipboard_outbox(&mut self) -> Vec<String> {
        std::mem::take(&mut self.clipboard_outbox)
    }

    /// Drain queued intents. Host calls this once per render frame.
    pub fn drain_intents(&mut self) -> Vec<BuffrEditIntent> {
        std::mem::take(&mut self.intents)
    }
}

impl Host for BuffrHost {
    type Intent = BuffrEditIntent;

    fn write_clipboard(&mut self, text: String) {
        self.clipboard_outbox.push(text);
    }

    fn read_clipboard(&mut self) -> Option<String> {
        self.clipboard_cache.clone()
    }

    fn now(&self) -> std::time::Duration {
        self.started.elapsed()
    }

    fn prompt_search(&mut self) -> Option<String> {
        // CEF prompt overlay is wired in phase 3 of buffr's roadmap.
        // Until then, abort the search rather than block on a sync
        // prompt the host can't service.
        None
    }

    fn emit_cursor_shape(&mut self, shape: CursorShape) {
        self.last_cursor_shape = shape;
    }

    fn emit_intent(&mut self, intent: Self::Intent) {
        self.intents.push(intent);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_outbox_drains() {
        let mut host = BuffrHost::new();
        host.write_clipboard("foo".into());
        host.write_clipboard("bar".into());
        let drained = host.drain_clipboard_outbox();
        assert_eq!(drained, vec!["foo".to_string(), "bar".to_string()]);
        assert!(host.drain_clipboard_outbox().is_empty());
    }

    #[test]
    fn read_clipboard_uses_cache() {
        let mut host = BuffrHost::new();
        assert_eq!(host.read_clipboard(), None);
        host.set_clipboard_cache(Some("payload".into()));
        assert_eq!(host.read_clipboard().as_deref(), Some("payload"));
    }

    #[test]
    fn intents_drain() {
        let mut host = BuffrHost::new();
        host.emit_intent(BuffrEditIntent::RequestAutocomplete);
        host.emit_intent(BuffrEditIntent::PassThrough);
        let drained = host.drain_intents();
        assert_eq!(drained.len(), 2);
        assert!(host.drain_intents().is_empty());
    }

    #[test]
    fn now_advances() {
        let host = BuffrHost::new();
        let t0 = host.now();
        std::thread::sleep(std::time::Duration::from_millis(1));
        let t1 = host.now();
        assert!(t1 > t0);
    }

    #[test]
    fn cursor_shape_recorded() {
        let mut host = BuffrHost::new();
        assert_eq!(host.last_cursor_shape, CursorShape::Block);
        host.emit_cursor_shape(CursorShape::Bar);
        assert_eq!(host.last_cursor_shape, CursorShape::Bar);
    }

    /// Compile-time check that BuffrHost satisfies the Host trait
    /// bound — confirms `type Intent = BuffrEditIntent` plus the full
    /// method set.
    #[test]
    fn satisfies_host_trait() {
        fn assert_host<H: Host>() {}
        assert_host::<BuffrHost>();
    }
}
