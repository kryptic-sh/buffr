//! CEF audio-stream tracking for the OSR sleep policy.
//!
//! [`BuffrAudioHandler`] wires into the CEF `AudioHandler` interface and
//! maintains a per-browser stream reference count.  When the count for a
//! given browser transitions 0→1 (stream started) or N→0 (last stream
//! stopped) it pushes an [`AudioEvent`] onto a shared queue so the UI
//! thread can update its [`AppState::media_active`] flag without crossing
//! a mutex on every `about_to_wait` tick.
//!
//! ## Design notes
//!
//! - `AudioStateSink` is `Arc<Mutex<...>>` keyed by CEF browser id (`i32`).
//! - `AudioEvent` is a simple plain struct; the queue is a `VecDeque`.
//! - The handler only cares about started/stopped; packet delivery is a
//!   no-op (we do not capture audio data).
//! - Incrementing on *started* and decrementing on *stopped* handles
//!   multi-stream pages (e.g. several `<audio>` elements playing
//!   simultaneously).  Only the edge transitions produce events so the
//!   queue stays small.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use cef::*;

// ── Shared state ──────────────────────────────────────────────────────────────

/// Per-browser audio state tracked by [`BuffrAudioHandler`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AudioState {
    /// `true` while at least one audio stream is active.
    pub active: bool,
    /// Running count of open streams for this browser.
    pub stream_count: u32,
}

/// Thread-safe map from CEF browser id to [`AudioState`].
pub type AudioStateSink = Arc<Mutex<HashMap<i32, AudioState>>>;

/// Edge event pushed onto the queue whenever a browser's `active` bit
/// flips.  Only one event per flip — not one per stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioEvent {
    pub browser_id: i32,
    pub active: bool,
}

/// Thread-safe queue of [`AudioEvent`]s drained by the UI thread each tick.
pub type AudioEventQueue = Arc<Mutex<VecDeque<AudioEvent>>>;

/// Construct an empty [`AudioStateSink`].
pub fn new_audio_state_sink() -> AudioStateSink {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Construct an empty [`AudioEventQueue`].
pub fn new_audio_event_queue() -> AudioEventQueue {
    Arc::new(Mutex::new(VecDeque::new()))
}

/// Drain all pending [`AudioEvent`]s from `queue`.  Returns an empty `Vec`
/// when the mutex is poisoned.
pub fn drain_audio_events(queue: &AudioEventQueue) -> Vec<AudioEvent> {
    if let Ok(mut g) = queue.lock() {
        return g.drain(..).collect();
    }
    Vec::new()
}

/// Snapshot of whether *any* browser in `sink` has an active stream.
pub fn any_audio_active(sink: &AudioStateSink) -> bool {
    sink.lock()
        .map(|g| g.values().any(|s| s.active))
        .unwrap_or(false)
}

/// Decrement the browser's active-stream count and report whether it
/// hit zero — the N→0 edge. Returns `true` exactly when the browser
/// became inactive, so the caller emits one `active: false` event.
///
/// Saturated at zero, so an error/stopped callback that races ahead of
/// its `started` cannot drive the count negative or emit a spurious
/// edge from an already-inactive browser.
fn decrement_stream(entry: &mut AudioState) -> bool {
    entry.stream_count = entry.stream_count.saturating_sub(1);
    if entry.stream_count == 0 && entry.active {
        entry.active = false;
        true
    } else {
        false
    }
}

// ── CEF handler ───────────────────────────────────────────────────────────────

wrap_audio_handler! {
    pub struct BuffrAudioHandler {
        sink: AudioStateSink,
        queue: AudioEventQueue,
    }

    impl AudioHandler {
        fn on_audio_stream_started(
            &self,
            browser: Option<&mut Browser>,
            _params: Option<&AudioParameters>,
            _channels: ::std::os::raw::c_int,
        ) {
            let browser_id = browser.map(|b| b.identifier()).unwrap_or(-1);
            let was_active = {
                let Ok(mut map) = self.sink.lock() else { return };
                let entry = map.entry(browser_id).or_default();
                let prev = entry.active;
                entry.stream_count = entry.stream_count.saturating_add(1);
                entry.active = true;
                prev
            };
            // Only push an event on the 0→1 edge.
            if !was_active
                && let Ok(mut q) = self.queue.lock()
            {
                q.push_back(AudioEvent {
                    browser_id,
                    active: true,
                });
            }
            tracing::debug!(
                target: "buffr_core::audio",
                browser_id,
                "on_audio_stream_started"
            );
        }

        fn on_audio_stream_stopped(&self, browser: Option<&mut Browser>) {
            let browser_id = browser.map(|b| b.identifier()).unwrap_or(-1);
            let became_inactive = {
                let Ok(mut map) = self.sink.lock() else { return };
                decrement_stream(map.entry(browser_id).or_default())
            };
            // Only push an event on the N→0 edge.
            if became_inactive
                && let Ok(mut q) = self.queue.lock()
            {
                q.push_back(AudioEvent {
                    browser_id,
                    active: false,
                });
            }
            tracing::debug!(
                target: "buffr_core::audio",
                browser_id,
                "on_audio_stream_stopped"
            );
        }

        fn on_audio_stream_error(
            &self,
            browser: Option<&mut Browser>,
            _message: Option<&CefString>,
        ) {
            // Treat an error the same as a stopped stream so we don't
            // leave the browser permanently marked active after an error.
            // Decrement, don't wipe: with several streams playing, one
            // errored stream must not clear the browser's active state
            // while its siblings are still producing audio.
            let browser_id = browser.map(|b| b.identifier()).unwrap_or(-1);
            let became_inactive = {
                let Ok(mut map) = self.sink.lock() else { return };
                decrement_stream(map.entry(browser_id).or_default())
            };
            if became_inactive
                && let Ok(mut q) = self.queue.lock()
            {
                q.push_back(AudioEvent {
                    browser_id,
                    active: false,
                });
            }
            tracing::debug!(
                target: "buffr_core::audio",
                browser_id,
                "on_audio_stream_error: treating as stopped"
            );
        }
    }
}

impl BuffrAudioHandler {
    /// Convenience constructor that wires both shared handles into the handler
    /// and returns the CEF-compatible `AudioHandler` wrapper.
    pub fn make(sink: AudioStateSink, queue: AudioEventQueue) -> AudioHandler {
        Self::new(sink, queue)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decrement_from_one_emits_edge_and_clears_active() {
        let mut entry = AudioState {
            active: true,
            stream_count: 1,
        };
        assert!(decrement_stream(&mut entry));
        assert!(!entry.active);
        assert_eq!(entry.stream_count, 0);
    }

    #[test]
    fn decrement_above_one_keeps_active() {
        // Two streams playing; one errors out. The browser must stay
        // active until the sibling stops too — the R1 fix.
        let mut entry = AudioState {
            active: true,
            stream_count: 2,
        };
        assert!(!decrement_stream(&mut entry));
        assert!(entry.active);
        assert_eq!(entry.stream_count, 1);
        assert!(decrement_stream(&mut entry));
        assert!(!entry.active);
    }

    #[test]
    fn decrement_from_zero_is_saturated() {
        // An error/stopped that races ahead of its started must not
        // drive the count negative or emit a spurious edge.
        let mut entry = AudioState::default();
        assert!(!decrement_stream(&mut entry));
        assert_eq!(entry.stream_count, 0);
        assert!(!entry.active);
    }

    #[test]
    fn error_and_stopped_share_the_same_edges() {
        // Wipe-vs-decrement was the whole bug; pin that both callbacks
        // route through the identical transition.
        let mut error_path = AudioState {
            active: true,
            stream_count: 3,
        };
        let mut stopped_path = error_path.clone();
        assert_eq!(
            decrement_stream(&mut error_path),
            decrement_stream(&mut stopped_path)
        );
        assert_eq!(error_path, stopped_path);
    }
}
