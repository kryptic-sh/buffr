//! Edit mode — CEF → Rust IPC plumbing for text-field focus/blur/mutate.
//!
//! ## Architecture
//!
//! Mirrors the console-log scraping pattern from [`crate::hint`]:
//!
//! 1. `edit.js` is injected into every main frame on `on_load_end` (once
//!    per page load, not per hint-mode invocation).
//! 2. The JS installs capture-phase `focusin`, `focusout`, and `input`
//!    listeners that emit `__buffr_edit__:<nonce>:{…}` lines via
//!    `console.log`.
//! 3. [`crate::handlers::BuffrDisplayHandler::on_console_message`]
//!    strips the sentinel, checks the nonce, parses the JSON tail via
//!    [`parse_console_event`], and pushes the result into an
//!    [`EditEventSink`] queue.
//!
//! The `<nonce>` is minted per main-frame load (see
//! [`crate::console_nonce`]) and spliced into the asset by
//! [`build_inject_script`]. Without it, any frame on the page could emit
//! `__buffr_edit__:{"type":"selection","value":"…"}` and push text of its
//! choosing into the yank-to-clipboard path.
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
    #[error("edit payload too large: {0}")]
    PayloadTooLarge(&'static str),
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

// ---- payload size limits -------------------------------------------------
//
// The page nonce is readable by the page itself, so a hostile top frame can
// emit authentic edit lines of arbitrary size (sentinel-prefixed lines also
// bypass the `CONSOLE_LOG_MAX_LEN` redaction in the display handler). Cap
// each field so a single event can't be attacker-sized (A4).

/// Maximum byte length of a `field_id` in an edit-event payload.
pub const EDIT_FIELD_ID_MAX_LEN: usize = 512;

/// Maximum byte length of a `value` in an edit-event payload.
pub const EDIT_VALUE_MAX_LEN: usize = 256 * 1024;

/// Try to parse a console message line as an edit-mode event.
///
/// `nonce` is the page nonce currently minted for the emitting browser
/// (`ConsoleNonces::page`).
///
/// Returns:
/// - `None` — the line is not an authentic edit line for `nonce`: no
///   [`EDIT_CONSOLE_SENTINEL`] at the *start*, or the nonce doesn't match.
///   The caller should treat it as a regular console message.
/// - `Some(Ok(event))` — authentic; JSON decoded successfully.
/// - `Some(Err(err))` — authentic but decoding failed; callers should log
///   the error rather than silently dropping it.
pub fn parse_console_event(
    line: &str,
    nonce: &str,
) -> Option<Result<EditConsoleEvent, ParseError>> {
    let suffix = crate::console_sentinel::sentinel_payload(line, EDIT_CONSOLE_SENTINEL, nonce)?;
    Some(parse_payload(suffix))
}

/// Decode a bare edit-event JSON payload (no sentinel, no nonce).
///
/// For backends that receive the payload over a trusted channel instead of
/// scraping `console.log`, so they don't have to synthesise a wire line.
pub fn parse_payload(json: &str) -> Result<EditConsoleEvent, ParseError> {
    // Two-pass approach: first extract the "type" discriminant, then
    // deserialise the full payload into the appropriate variant. Avoids
    // a custom Visitor while keeping good error messages.
    let tag: TypeTag = serde_json::from_str(json)?;

    let event = match tag.kind.as_str() {
        "focus" => {
            let r: RawFocus = serde_json::from_str(json)?;
            if r.field_id.len() > EDIT_FIELD_ID_MAX_LEN {
                return Err(ParseError::PayloadTooLarge("field_id"));
            }
            if r.value.len() > EDIT_VALUE_MAX_LEN {
                return Err(ParseError::PayloadTooLarge("value"));
            }
            EditConsoleEvent::Focus {
                field_id: r.field_id,
                kind: r.kind,
                value: r.value,
                selection_start: r.selection_start,
                selection_end: r.selection_end,
            }
        }
        "blur" => {
            let r: RawBlur = serde_json::from_str(json)?;
            if r.field_id.len() > EDIT_FIELD_ID_MAX_LEN {
                return Err(ParseError::PayloadTooLarge("field_id"));
            }
            EditConsoleEvent::Blur {
                field_id: r.field_id,
            }
        }
        "mutate" => {
            let r: RawMutate = serde_json::from_str(json)?;
            if r.field_id.len() > EDIT_FIELD_ID_MAX_LEN {
                return Err(ParseError::PayloadTooLarge("field_id"));
            }
            if r.value.len() > EDIT_VALUE_MAX_LEN {
                return Err(ParseError::PayloadTooLarge("value"));
            }
            EditConsoleEvent::Mutate {
                field_id: r.field_id,
                value: r.value,
            }
        }
        "selection" => {
            let r: RawSelection = serde_json::from_str(json)?;
            if r.value.len() > EDIT_VALUE_MAX_LEN {
                return Err(ParseError::PayloadTooLarge("value"));
            }
            EditConsoleEvent::Selection { value: r.value }
        }
        other => {
            return Err(ParseError::UnknownType(other.to_owned()));
        }
    };

    Ok(event)
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

/// Maximum number of [`EditConsoleEvent`]s held in the sink at once.
///
/// The producer is the CEF/WebKit callback thread and the drain runs on
/// the UI tick, which parks while the window is occluded. A hostile top
/// frame can emit events as fast as it likes (the page nonce is readable
/// by the page itself), so an unbounded queue would grow without limit
/// while the drain is stalled. Drop the oldest so the UI always sees the
/// most recent events (A4).
pub const EDIT_EVENT_SINK_CAP: usize = 1024;

/// Push one event onto the sink, dropping the oldest when the queue is at
/// capacity. Mirrors the context-menu sink's drop-oldest policy
/// (`CONTEXT_MENU_REQUEST_QUEUE_CAP`).
pub fn push_edit_event(sink: &EditEventSink, event: EditConsoleEvent) {
    if let Ok(mut guard) = sink.lock() {
        if guard.len() >= EDIT_EVENT_SINK_CAP {
            guard.pop_front();
        }
        guard.push_back(event);
    }
}

/// Build the JS string that `frame.execute_java_script` will execute.
///
/// Substitutes the two placeholders the asset uses:
///
/// - `%%SENTINEL%%`     → [`EDIT_CONSOLE_SENTINEL`] + `nonce` + `:`, the
///   exact prefix [`parse_console_event`] will accept for this page load.
/// - `%%OVERLAY_CLASS%%` → [`EDIT_DOM_OVERLAY_CLASS`]
///
/// The asset already wraps the substitution sites in string literals so
/// no additional quoting is needed here (all values are ASCII-safe;
/// `nonce` is plain hex).
///
/// `nonce` comes from [`crate::console_nonce::ConsoleNonces::rotate_page`].
/// Inject the result into a **main frame only** — handing a subframe the
/// nonce would give away the very thing it is there to withhold.
///
/// Re-injection into a document that already has `edit.js` wired is
/// handled by the asset's `window.__buffrEditTeardown` hook: the new copy
/// unwires the old listeners and installs its own, so a soft-navigation
/// `on_load_end` that rotates the nonce does not leave the document
/// emitting the stale one (which Rust would then drop, silently killing
/// edit mode). Teardown takes no arguments, so nothing on that path can
/// hand the nonce to page-controlled code.
pub fn build_inject_script(nonce: &str) -> String {
    include_str!("../assets/edit.js")
        .replace(
            "%%SENTINEL%%",
            &crate::console_sentinel::sentinel_prefix(EDIT_CONSOLE_SENTINEL, nonce),
        )
        .replace("%%OVERLAY_CLASS%%", EDIT_DOM_OVERLAY_CLASS)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- parse_console_event --------------------------------------------

    const NONCE: &str = "0123456789abcdef0123456789abcdef";

    fn wire(body: &str) -> String {
        format!("{EDIT_CONSOLE_SENTINEL}{NONCE}:{body}")
    }

    #[test]
    fn parse_non_sentinel() {
        // Lines that don't start with the sentinel return None.
        assert!(parse_console_event("hello world", NONCE).is_none());
        assert!(parse_console_event(&format!("__buffr_hint__:{NONCE}:{{}}"), NONCE).is_none());
        assert!(parse_console_event("", NONCE).is_none());
    }

    #[test]
    fn parse_focus_event() {
        let line = wire(
            r#"{"type":"focus","field_id":"f1","kind":"input","value":"hello","selection_start":5,"selection_end":5}"#,
        );
        let ev = parse_console_event(&line, NONCE)
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
        let line = wire(
            r#"{"type":"focus","field_id":"f2","kind":"contentEditable","value":"world","selection_start":null,"selection_end":null}"#,
        );
        let ev = parse_console_event(&line, NONCE)
            .expect("Some")
            .expect("ok");
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
        let line = wire(r#"{"type":"blur","field_id":"f3"}"#);
        let ev = parse_console_event(&line, NONCE)
            .expect("Some")
            .expect("ok");
        match ev {
            EditConsoleEvent::Blur { field_id } => assert_eq!(field_id, "f3"),
            other => panic!("expected Blur, got {other:?}"),
        }
    }

    #[test]
    fn parse_mutate_event() {
        let line = wire(r#"{"type":"mutate","field_id":"f4","value":"new text"}"#);
        let ev = parse_console_event(&line, NONCE)
            .expect("Some")
            .expect("ok");
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
        let line = wire(r#"{"type":"weird","field_id":"f5"}"#);
        let result = parse_console_event(&line, NONCE).expect("Some");
        assert!(result.is_err(), "expected Err for unknown type, got Ok");
        match result.unwrap_err() {
            ParseError::UnknownType(t) => assert_eq!(t, "weird"),
            other => panic!("expected UnknownType, got {other:?}"),
        }
    }

    #[test]
    fn parse_malformed_json() {
        let line = wire("not json at all");
        let result = parse_console_event(&line, NONCE).expect("Some");
        assert!(result.is_err(), "expected Err for malformed JSON");
    }

    // ---- H5: forged edit events ------------------------------------------

    #[test]
    fn parse_rejects_line_without_nonce() {
        // The pre-nonce wire format: any frame could emit this to push
        // attacker-chosen text into the yank-to-clipboard path.
        let forged = r#"__buffr_edit__:{"type":"selection","value":"attacker text"}"#;
        assert!(parse_console_event(forged, NONCE).is_none());
    }

    #[test]
    fn parse_rejects_wrong_nonce() {
        let forged = format!(
            "{EDIT_CONSOLE_SENTINEL}{}:{}",
            "f".repeat(32),
            r#"{"type":"selection","value":"attacker text"}"#
        );
        assert!(parse_console_event(&forged, NONCE).is_none());
    }

    #[test]
    fn parse_rejects_unanchored_sentinel() {
        let forged = format!("%cINFO {}", wire(r#"{"type":"blur","field_id":"f3"}"#));
        assert!(parse_console_event(&forged, NONCE).is_none());
    }

    #[test]
    fn parse_rejects_nonce_from_a_previous_page_load() {
        use crate::console_nonce::ConsoleNonces;
        let nonces = ConsoleNonces::new();
        let old = nonces.rotate_page(1);
        let line = format!(
            "{EDIT_CONSOLE_SENTINEL}{old}:{}",
            r#"{"type":"blur","field_id":"f3"}"#
        );
        assert!(parse_console_event(&line, &old).is_some(), "sanity");
        let new = nonces.rotate_page(1);
        assert!(
            parse_console_event(&line, &new).is_none(),
            "a nonce leaked on a prior load must not work after navigation"
        );
    }

    // ---- parse_payload ----------------------------------------------------

    #[test]
    fn parse_payload_skips_the_wire_framing() {
        let ev = parse_payload(r#"{"type":"blur","field_id":"f9"}"#).expect("ok");
        assert!(matches!(ev, EditConsoleEvent::Blur { field_id } if field_id == "f9"));
    }

    // ---- build_inject_script --------------------------------------------

    #[test]
    fn build_inject_script_substitutes_placeholders() {
        let script = build_inject_script(NONCE);
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

    #[test]
    fn build_inject_script_emits_the_prefix_parse_accepts() {
        let script = build_inject_script(NONCE);
        let prefix = format!("{EDIT_CONSOLE_SENTINEL}{NONCE}:");
        assert!(script.contains(&prefix), "nonce not spliced into edit.js");
        let emitted = format!("{prefix}{}", r#"{"type":"blur","field_id":"f1"}"#);
        assert!(parse_console_event(&emitted, NONCE).unwrap().is_ok());
    }

    #[test]
    fn build_inject_script_differs_across_loads() {
        use crate::console_nonce::new_console_nonce;
        let a = build_inject_script(&new_console_nonce());
        let b = build_inject_script(&new_console_nonce());
        assert_ne!(a, b, "nonce must change across page loads");
    }

    #[test]
    fn build_inject_script_rewires_an_already_wired_document() {
        // The `__buffrEditWired` guard must not leave a soft-navigated
        // document emitting the stale nonce.
        let script = build_inject_script(NONCE);
        assert!(
            script.contains("__buffrEditTeardown"),
            "edit.js lost its teardown hook — a soft-nav re-injection would \
             silently kill edit mode for that document"
        );
    }

    #[test]
    fn edit_js_never_publishes_the_nonce_on_window() {
        // The nonce must stay in the IIFE's scope. Any `window.` /
        // `self.` assignment carrying SENTINEL would hand it to the page
        // outright.
        let script = build_inject_script(NONCE);
        for line in script.lines() {
            let code = line.trim_start();
            if code.starts_with("//") {
                continue;
            }
            assert!(
                !(code.contains("window.") && code.contains("SENTINEL =")),
                "edit.js assigns the sentinel onto window: {line}"
            );
        }
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

    #[test]
    fn push_edit_event_drops_oldest_at_cap() {
        let sink = new_edit_event_sink();
        for i in 0..(EDIT_EVENT_SINK_CAP + 5) {
            push_edit_event(
                &sink,
                EditConsoleEvent::Blur {
                    field_id: format!("f{i}"),
                },
            );
            // The queue must never exceed the cap at any point.
            let len = sink.lock().unwrap().len();
            assert!(
                len <= EDIT_EVENT_SINK_CAP,
                "queue exceeded cap after push {i}: len {len}"
            );
        }
        let drained = drain_edit_events(&sink);
        assert_eq!(drained.len(), EDIT_EVENT_SINK_CAP);
        // The oldest 5 were dropped; the first survivor is the 6th pushed.
        assert!(
            matches!(&drained[0], EditConsoleEvent::Blur { field_id } if field_id == "f5"),
            "expected the 6th-pushed event first, got {:?}",
            drained[0]
        );
        assert!(
            matches!(&drained[EDIT_EVENT_SINK_CAP - 1], EditConsoleEvent::Blur { field_id } if field_id == &format!("f{}", EDIT_EVENT_SINK_CAP + 4)),
            "expected the last-pushed event last"
        );
    }

    #[test]
    fn parse_payload_rejects_oversized_field_id_and_value() {
        // Oversized field_id on a focus payload → hard error, event dropped.
        let big_id = "x".repeat(EDIT_FIELD_ID_MAX_LEN + 1);
        let payload = format!(
            r#"{{"type":"focus","field_id":"{big_id}","kind":"input","value":"v","selection_start":null,"selection_end":null}}"#
        );
        match parse_payload(&payload) {
            Err(ParseError::PayloadTooLarge(what)) => assert_eq!(what, "field_id"),
            other => panic!("expected PayloadTooLarge(field_id), got {other:?}"),
        }

        // Oversized value on a focus payload → hard error, event dropped.
        let big_value = "y".repeat(EDIT_VALUE_MAX_LEN + 1);
        let payload = format!(
            r#"{{"type":"focus","field_id":"f","kind":"input","value":"{big_value}","selection_start":null,"selection_end":null}}"#
        );
        match parse_payload(&payload) {
            Err(ParseError::PayloadTooLarge(what)) => assert_eq!(what, "value"),
            other => panic!("expected PayloadTooLarge(value), got {other:?}"),
        }

        // A normal payload still parses.
        let ok = parse_payload(
            r#"{"type":"focus","field_id":"f","kind":"input","value":"hello","selection_start":0,"selection_end":0}"#,
        )
        .expect("normal payload must still parse");
        assert!(matches!(ok, EditConsoleEvent::Focus { .. }));
    }
}
