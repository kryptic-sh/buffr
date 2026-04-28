//! Edit mode — CEF → Rust IPC plumbing for text-field focus/blur/mutate.
//!
//! ## Architecture
//!
//! Mirrors the console-log scraping pattern from [`crate::hint`]:
//!
//! 1. `edit.js` is injected into every main frame on `on_load_end` (once
//!    per page load, not per hint-mode invocation).
//! 2. The JS installs capture-phase `focusin`, `focusout`, and `input`
//!    listeners that emit `%%SENTINEL%%:{…}` lines via `console.log`.
//! 3. [`crate::handlers::BuffrDisplayHandler::on_console_message`]
//!    strips the sentinel, parses the JSON tail via
//!    [`parse_console_event`], and pushes the result into an
//!    [`EditEventSink`] queue.
//! 4. Stage 2 will drain the queue from the UI render loop and wire events
//!    into [`EditSession`] construction / keystroke routing / Esc handling.
//!
//! ## Why a queue, not a single-slot mailbox?
//!
//! [`crate::hint::HintEventSink`] is `Mutex<Option<_>>` because hint mode
//! only ever has one meaningful "ready" message per session — overwriting
//! a stale duplicate is correct. Edit events must not drop predecessors:
//! a rapid `focus → blur → focus` sequence contains three meaningful events,
//! and dropping the middle one would leave Stage 2 out of sync with the
//! actual field state. We use `VecDeque` so bursts are queued in order.
//!
//! ## Stage 2 TODO
//!
//! Stage 2 will add:
//! - `window.__buffrEditApply(field_id, value, [start, end])` — push a
//!   new value + caret from Rust back into the focused field.
//! - `window.__buffrEditDetach(field_id)` — remove the active class and
//!   stop forwarding input events for this field.
//! - Keystroke routing: `i`/`a`/`I`/`A` open an [`EditSession`] seeded
//!   from the `Focus` event's `value`; `<Esc>` closes it and calls detach.
//! - Per-frame drain of `EditSession::take_content_change()` → DOM update.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Sentinel that prefixes every edit-mode console message.
///
/// The display handler scans every incoming console line for this prefix;
/// only lines that start with it are decoded as edit events.
pub const EDIT_CONSOLE_SENTINEL: &str = "__buffr_edit__:";

/// CSS class applied to the currently-focused editable field.
///
/// Declared here (not just in `edit.js`) so Stage 2 user-CSS blocks can
/// reference the name without a follow-up edit to the JS asset.
///
/// Stage 2 will style this class to give the user visual feedback that
/// buffr's edit mode is active on the field (e.g. a coloured focus ring).
pub const EDIT_DOM_OVERLAY_CLASS: &str = "buffr-edit-active";

/// Errors that can occur when parsing an edit-mode console line.
#[derive(Debug, Error)]
pub enum ParseError {
    #[error("JSON parse failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unknown event type: {0:?}")]
    UnknownType(String),
}

/// Coarse classification of the focused field. Drives Stage 2's DOM
/// mutation strategy:
///
/// - [`Input`](EditFieldKind::Input) — `el.value = …; el.dispatchEvent(…)`
/// - [`Textarea`](EditFieldKind::Textarea) — same as `Input`.
/// - [`ContentEditable`](EditFieldKind::ContentEditable) — set
///   `el.innerText` and rebuild the selection range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EditFieldKind {
    Input,
    Textarea,
    ContentEditable,
}

/// Raw event variants emitted by `edit.js` and decoded on the Rust side.
///
/// All variants carry `field_id` — the JS-minted stable per-element
/// identifier — so the Rust side can match events to the same element
/// across a `Focus → Mutate* → Blur` sequence.
#[derive(Debug, Clone, PartialEq)]
pub enum EditConsoleEvent {
    /// The user focused an editable field.
    ///
    /// Carries the initial value and caret positions so Stage 2 can seed
    /// an `EditSession` without a separate DOM read round-trip.
    Focus {
        field_id: String,
        kind: EditFieldKind,
        value: String,
        /// Caret start index. `None` for `contentEditable` fields (the JS
        /// side cannot cheaply compute a flat index for a Range).
        selection_start: Option<u32>,
        /// Caret end index (same caveat as `selection_start`).
        selection_end: Option<u32>,
    },
    /// The user moved focus away from an editable field.
    Blur { field_id: String },
    /// The page changed the field's value while buffr was attached —
    /// covers OS paste, IME composition commit, and browser autocomplete.
    ///
    /// Stage 2 reconciles the incoming value against `EditSession`'s rope
    /// and re-derives the diff so undo history stays correct. Stage 1
    /// just queues it.
    Mutate { field_id: String, value: String },
    /// On-demand snapshot of `window.getSelection().toString()` emitted
    /// by `__buffrEmitSelection`. Used by the apps layer to land a
    /// Visual-mode yank into the system clipboard via hjkl-clipboard
    /// instead of routing through Chromium's internal copy command.
    Selection { value: String },
}

// ---- wire types for serde ----------------------------------------------
//
// We can't derive `Deserialize` directly on `EditConsoleEvent` because
// the JSON uses `type` (a Rust keyword) as the discriminant field and the
// variants have heterogeneous payloads. Per-variant wire structs handle
// the impedance mismatch cleanly.
//
// Each variant's JSON shape:
//
//   focus:  { type:"focus",  field_id, kind, value, selection_start?, selection_end? }
//   blur:   { type:"blur",   field_id }
//   mutate: { type:"mutate", field_id, value }

#[derive(Deserialize)]
struct RawFocus {
    field_id: String,
    kind: EditFieldKind,
    value: String,
    selection_start: Option<u32>,
    selection_end: Option<u32>,
}

#[derive(Deserialize)]
struct RawBlur {
    field_id: String,
}

#[derive(Deserialize)]
struct RawMutate {
    field_id: String,
    value: String,
}

#[derive(Deserialize)]
struct RawSelection {
    value: String,
}

#[derive(Deserialize)]
struct TypeTag {
    #[serde(rename = "type")]
    kind: String,
}

/// Try to parse a console message line as an edit-mode event.
///
/// Returns:
/// - `None` — line does not carry the [`EDIT_CONSOLE_SENTINEL`] prefix;
///   the caller should treat it as a regular console message.
/// - `Some(Ok(event))` — prefix present; JSON decoded successfully.
/// - `Some(Err(err))` — prefix present but decoding failed; callers
///   should log the error rather than silently dropping it.
pub fn parse_console_event(line: &str) -> Option<Result<EditConsoleEvent, ParseError>> {
    // Some pages (monkeytype, etc.) wrap `console.log` to prepend their
    // own styling format string (e.g. `%cINFO ...`). Find the sentinel
    // anywhere in the line, not just at the start.
    let idx = line.find(EDIT_CONSOLE_SENTINEL)?;
    let suffix = &line[idx + EDIT_CONSOLE_SENTINEL.len()..];

    // Two-pass approach: first extract the "type" discriminant, then
    // deserialise the full payload into the appropriate variant. Avoids
    // a custom Visitor while keeping good error messages.
    let tag: TypeTag = match serde_json::from_str(suffix) {
        Ok(t) => t,
        Err(e) => return Some(Err(ParseError::Json(e))),
    };

    let event = match tag.kind.as_str() {
        "focus" => {
            let r: RawFocus = match serde_json::from_str(suffix) {
                Ok(v) => v,
                Err(e) => return Some(Err(ParseError::Json(e))),
            };
            EditConsoleEvent::Focus {
                field_id: r.field_id,
                kind: r.kind,
                value: r.value,
                selection_start: r.selection_start,
                selection_end: r.selection_end,
            }
        }
        "blur" => {
            let r: RawBlur = match serde_json::from_str(suffix) {
                Ok(v) => v,
                Err(e) => return Some(Err(ParseError::Json(e))),
            };
            EditConsoleEvent::Blur {
                field_id: r.field_id,
            }
        }
        "mutate" => {
            let r: RawMutate = match serde_json::from_str(suffix) {
                Ok(v) => v,
                Err(e) => return Some(Err(ParseError::Json(e))),
            };
            EditConsoleEvent::Mutate {
                field_id: r.field_id,
                value: r.value,
            }
        }
        "selection" => {
            let r: RawSelection = match serde_json::from_str(suffix) {
                Ok(v) => v,
                Err(e) => return Some(Err(ParseError::Json(e))),
            };
            EditConsoleEvent::Selection { value: r.value }
        }
        other => {
            return Some(Err(ParseError::UnknownType(other.to_owned())));
        }
    };

    Some(Ok(event))
}

/// Queue shared between [`crate::handlers::BuffrDisplayHandler`] (writer)
/// and the UI render loop (reader). Uses `VecDeque` so bursts of
/// `focus → mutate → blur` events are preserved in order — unlike the
/// hint sink which overwrites with a single slot.
pub type EditEventSink = Arc<Mutex<VecDeque<EditConsoleEvent>>>;

/// Construct a fresh, empty [`EditEventSink`].
pub fn new_edit_event_sink() -> EditEventSink {
    Arc::new(Mutex::new(VecDeque::new()))
}

/// Drain all queued events, returning them in arrival order.
///
/// Returns an empty `Vec` when the queue is empty or the lock is
/// poisoned. Callers should treat a poisoned lock as a no-op (the
/// render loop will retry next tick).
pub fn drain_edit_events(sink: &EditEventSink) -> Vec<EditConsoleEvent> {
    sink.lock()
        .map(|mut g| g.drain(..).collect())
        .unwrap_or_default()
}

/// Build the JS string that `frame.execute_java_script` will execute.
///
/// Substitutes the two placeholders the asset uses:
///
/// - `%%SENTINEL%%`     → [`EDIT_CONSOLE_SENTINEL`]
/// - `%%OVERLAY_CLASS%%` → [`EDIT_DOM_OVERLAY_CLASS`]
///
/// The asset already wraps the substitution sites in string literals so
/// no additional quoting is needed here (both values are ASCII-safe).
pub fn build_inject_script() -> String {
    include_str!("../assets/edit.js")
        .replace("%%SENTINEL%%", EDIT_CONSOLE_SENTINEL)
        .replace("%%OVERLAY_CLASS%%", EDIT_DOM_OVERLAY_CLASS)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- parse_console_event --------------------------------------------

    #[test]
    fn parse_non_sentinel() {
        // Lines that don't start with the sentinel return None.
        assert!(parse_console_event("hello world").is_none());
        assert!(parse_console_event("__buffr_hint__:{\"kind\":\"ready\"}").is_none());
        assert!(parse_console_event("").is_none());
    }

    #[test]
    fn parse_focus_event() {
        let line = r#"__buffr_edit__:{"type":"focus","field_id":"f1","kind":"input","value":"hello","selection_start":5,"selection_end":5}"#;
        let ev = parse_console_event(line)
            .expect("should return Some")
            .expect("should parse ok");
        match ev {
            EditConsoleEvent::Focus {
                field_id,
                kind,
                value,
                selection_start,
                selection_end,
            } => {
                assert_eq!(field_id, "f1");
                assert_eq!(kind, EditFieldKind::Input);
                assert_eq!(value, "hello");
                assert_eq!(selection_start, Some(5));
                assert_eq!(selection_end, Some(5));
            }
            other => panic!("expected Focus, got {other:?}"),
        }
    }

    #[test]
    fn parse_focus_event_null_selection() {
        // contentEditable fields emit null for selection positions.
        let line = r#"__buffr_edit__:{"type":"focus","field_id":"f2","kind":"contentEditable","value":"world","selection_start":null,"selection_end":null}"#;
        let ev = parse_console_event(line).expect("Some").expect("ok");
        match ev {
            EditConsoleEvent::Focus {
                selection_start,
                selection_end,
                ..
            } => {
                assert_eq!(selection_start, None);
                assert_eq!(selection_end, None);
            }
            other => panic!("expected Focus, got {other:?}"),
        }
    }

    #[test]
    fn parse_blur_event() {
        let line = r#"__buffr_edit__:{"type":"blur","field_id":"f3"}"#;
        let ev = parse_console_event(line).expect("Some").expect("ok");
        match ev {
            EditConsoleEvent::Blur { field_id } => assert_eq!(field_id, "f3"),
            other => panic!("expected Blur, got {other:?}"),
        }
    }

    #[test]
    fn parse_mutate_event() {
        let line = r#"__buffr_edit__:{"type":"mutate","field_id":"f4","value":"new text"}"#;
        let ev = parse_console_event(line).expect("Some").expect("ok");
        match ev {
            EditConsoleEvent::Mutate { field_id, value } => {
                assert_eq!(field_id, "f4");
                assert_eq!(value, "new text");
            }
            other => panic!("expected Mutate, got {other:?}"),
        }
    }

    #[test]
    fn parse_unknown_type() {
        // A payload with a valid sentinel but unrecognised `type` must
        // return `Some(Err(_))`, not `None` or `Some(Ok(_))`.
        let line = r#"__buffr_edit__:{"type":"weird","field_id":"f5"}"#;
        let result = parse_console_event(line).expect("Some");
        assert!(result.is_err(), "expected Err for unknown type, got Ok");
        match result.unwrap_err() {
            ParseError::UnknownType(t) => assert_eq!(t, "weird"),
            other => panic!("expected UnknownType, got {other:?}"),
        }
    }

    #[test]
    fn parse_malformed_json() {
        let line = "__buffr_edit__:not json at all";
        let result = parse_console_event(line).expect("Some");
        assert!(result.is_err(), "expected Err for malformed JSON");
    }

    // ---- build_inject_script --------------------------------------------

    #[test]
    fn build_inject_script_substitutes_placeholders() {
        let script = build_inject_script();
        // No raw placeholder markers should remain.
        assert!(
            !script.contains("%%SENTINEL%%"),
            "%%SENTINEL%% not substituted"
        );
        assert!(
            !script.contains("%%OVERLAY_CLASS%%"),
            "%%OVERLAY_CLASS%% not substituted"
        );
        // The actual values must appear.
        assert!(
            script.contains(EDIT_CONSOLE_SENTINEL),
            "sentinel not in script"
        );
        assert!(
            script.contains(EDIT_DOM_OVERLAY_CLASS),
            "overlay class not in script"
        );
        // No `%%` sequences should remain at all.
        assert!(!script.contains("%%"), "stray %% in script:\n{script}");
    }

    // ---- sink helpers ---------------------------------------------------

    #[test]
    fn drain_returns_in_order() {
        let sink = new_edit_event_sink();
        {
            let mut g = sink.lock().unwrap();
            g.push_back(EditConsoleEvent::Blur {
                field_id: "a".to_string(),
            });
            g.push_back(EditConsoleEvent::Blur {
                field_id: "b".to_string(),
            });
            g.push_back(EditConsoleEvent::Blur {
                field_id: "c".to_string(),
            });
        }
        let drained = drain_edit_events(&sink);
        assert_eq!(drained.len(), 3);
        // Order must be preserved.
        assert!(matches!(&drained[0], EditConsoleEvent::Blur { field_id } if field_id == "a"));
        assert!(matches!(&drained[1], EditConsoleEvent::Blur { field_id } if field_id == "b"));
        assert!(matches!(&drained[2], EditConsoleEvent::Blur { field_id } if field_id == "c"));
        // Sink is now empty.
        assert!(drain_edit_events(&sink).is_empty());
    }

    #[test]
    fn new_sink_is_empty() {
        let sink = new_edit_event_sink();
        assert!(drain_edit_events(&sink).is_empty());
    }
}
