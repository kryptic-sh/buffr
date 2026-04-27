// buffr edit mode -- focus/blur/mutate event bridge.
//
// This script is injected once per main-frame load via
// `LoadHandler::on_load_end → frame.execute_java_script`. It installs
// capture-phase listeners on `focusin`, `focusout`, and `input` so that
// every text-field interaction is visible regardless of whether the page
// calls `stopPropagation`.
//
// IPC (renderer → browser) uses `console.log` with a sentinel prefix:
// `%%SENTINEL%%` + JSON. The buffr `DisplayHandler::on_console_message`
// scrapes those lines and routes parsed events into the `EditEventSink`.
// This is the same console-log scraping pattern as hint.js — see
// `crates/buffr-core/src/hint.rs` for the rationale.
//
// IPC (browser → renderer) will be added in Stage 2:
//   `window.__buffrEditApply(field_id, value, [start, end])` — push a
//       new value + caret position from Rust back into the focused field.
//   `window.__buffrEditDetach(field_id)` — remove the active-class and
//       stop forwarding input events for this field.
//
// The `%%OVERLAY_CLASS%%` class is added to focused fields now so that
// Stage 2 can style them without a follow-up edit to this asset.
//
// Guard: `window.__buffrEditWired` prevents double-installation on
// SPA soft navigations that re-run injected scripts.

(function () {
    'use strict';

    if (window.__buffrEditWired) { return; }
    window.__buffrEditWired = true;

    var SENTINEL = '%%SENTINEL%%';
    var OVERLAY_CLASS = '%%OVERLAY_CLASS%%';

    // ---- stable per-element ID ------------------------------------------
    //
    // We need a stable string ID for each DOM element so the Rust side can
    // refer to the same field across focus/blur/mutate events. WeakMap
    // means the element can still be garbage-collected when the page
    // removes it; we never hold a strong reference.

    var idMap = new WeakMap();
    var nextId = 1;

    function idFor(el) {
        var id = idMap.get(el);
        if (id == null) {
            id = 'f' + (nextId++);
            idMap.set(el, id);
        }
        return id;
    }

    // ---- element classification -----------------------------------------
    //
    // Returns one of "input" | "textarea" | "contentEditable" | null.
    // null means "not a text-editable field; ignore".

    function kindOf(el) {
        if (!el || !el.tagName) { return null; }
        var tag = el.tagName.toUpperCase();
        if (tag === 'TEXTAREA') { return 'textarea'; }
        if (tag === 'INPUT') { return 'input'; }
        if (el.isContentEditable) { return 'contentEditable'; }
        return null;
    }

    // ---- current text value --------------------------------------------
    //
    // For <input>/<textarea> use the `.value` property (reflects the live
    // editable content, not the HTML attribute). For contentEditable, use
    // `.innerText` which preserves line breaks without HTML markup noise.

    function valueOf(el, kind) {
        if (kind === 'input' || kind === 'textarea') {
            return el.value || '';
        }
        if (kind === 'contentEditable') {
            return el.innerText || '';
        }
        return '';
    }

    // ---- IPC emit -------------------------------------------------------
    //
    // Wrap in try/catch so a console error in the outer listener can never
    // re-enter this function and produce an infinite loop.

    function emit(payload) {
        try {
            console.log(SENTINEL + JSON.stringify(payload));
        } catch (_) {}
    }

    // ---- focusin (capture) ----------------------------------------------
    //
    // Fires when any element receives focus, bubbles up from the target.
    // Capture phase (third arg = true) ensures we see it before any
    // page-level handlers that call stopPropagation.

    document.addEventListener('focusin', function (ev) {
        var el = ev.target;
        var kind = kindOf(el);
        if (!kind) { return; }

        var id = idFor(el);
        el.classList.add(OVERLAY_CLASS);

        // selectionStart/selectionEnd are only meaningful on <input> and
        // <textarea>; contentEditable caret is a Range, not an index.
        var start = (kind !== 'contentEditable') ? el.selectionStart : null;
        var end   = (kind !== 'contentEditable') ? el.selectionEnd   : null;

        emit({
            type: 'focus',
            field_id: id,
            kind: kind,
            value: valueOf(el, kind),
            selection_start: start,
            selection_end: end
        });
    }, true);

    // ---- focusout (capture) ---------------------------------------------
    //
    // Fires when any element loses focus. We remove the overlay class and
    // emit a blur event so Stage 2 can drop the EditSession.

    document.addEventListener('focusout', function (ev) {
        var el = ev.target;
        var kind = kindOf(el);
        if (!kind) { return; }

        var id = idFor(el);
        el.classList.remove(OVERLAY_CLASS);

        emit({ type: 'blur', field_id: id });
    }, true);

    // ---- input (capture) ------------------------------------------------
    //
    // Fires when the page changes a field's value — covers OS paste,
    // IME composition commit, browser autocomplete, and any JS that
    // dispatches a synthetic InputEvent. We only emit for fields that
    // are already in `idMap` (i.e. were previously focused by the user)
    // so random off-screen autofill doesn't produce noise.
    //
    // Stage 2 will gate on `window.__buffrEditAttached === id` to avoid
    // rebroadcasting our own DOM mutations (when Rust writes back into
    // the field the `input` event fires again; the gate prevents a
    // Rust-emits → JS-emits → Rust-processes loop).

    document.addEventListener('input', function (ev) {
        var el = ev.target;
        var kind = kindOf(el);
        if (!kind) { return; }

        // Only forward events for fields that already have a buffr ID.
        // `idMap.has` is a WeakMap lookup — O(1), no allocation.
        if (!idMap.has(el)) { return; }

        var id = idMap.get(el);
        emit({ type: 'mutate', field_id: id, value: valueOf(el, kind) });
    }, true);

})();
