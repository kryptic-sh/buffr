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
// IPC (browser → renderer) — Stage 2 additions:
//   `window.__buffrEditApply(field_id, value)` — push a new value from
//       Rust back into the focused field, firing a synthetic `input`
//       event so the page's bound handlers stay in sync.
//   `window.__buffrEditAttach(field_id)` — add the active CSS class.
//   `window.__buffrEditDetach(field_id)` — remove the active CSS class.
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

    // Strip any auto-focus the page applied on load. buffr starts every
    // page in Normal mode; the user explicitly enters Insert via `i`
    // or by clicking an input. Without this, sites like google.com
    // autofocus their search box and create the click-on-already-focused
    // race where DOM doesn't fire focusin.
    function clearInitialFocus() {
        var el = document.activeElement;
        if (el && el !== document.body && el !== document.documentElement
            && typeof el.blur === 'function') {
            el.blur();
        }
    }
    clearInitialFocus();
    if (document.readyState !== 'complete') {
        window.addEventListener('load', clearInitialFocus, { once: true });
    }

    // ---- stable per-element ID ------------------------------------------
    //
    // We need a stable string ID for each DOM element so the Rust side can
    // refer to the same field across focus/blur/mutate events. WeakMap
    // means the element can still be garbage-collected when the page
    // removes it; we never hold a strong reference.

    var idMap = new WeakMap();   // Element → id string (forward)
    var elById = new Map();      // id string → WeakRef<Element> (reverse)
    var nextId = 1;

    function idFor(el) {
        var id = idMap.get(el);
        if (id == null) {
            id = 'f' + (nextId++);
            idMap.set(el, id);
            elById.set(id, new WeakRef(el));
        }
        return id;
    }

    // Re-resolve an element by id; cleans the map if the element was GC'd.
    function elFor(id) {
        var ref = elById.get(id);
        if (!ref) { return null; }
        var el = ref.deref();
        if (!el) { elById.delete(id); return null; }
        return el;
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
    // Gate: if `el.__buffrApplying` is set, the mutation originated from
    // our own `__buffrEditApply` call — skip re-emitting to break the
    // Rust-writes → JS-emits → Rust-processes loop.

    document.addEventListener('input', function (ev) {
        var el = ev.target;
        var kind = kindOf(el);
        if (!kind) { return; }

        // Only forward events for fields that already have a buffr ID.
        // `idMap.has` is a WeakMap lookup — O(1), no allocation.
        if (!idMap.has(el)) { return; }

        // Skip echoes of our own DOM writes.
        if (el.__buffrApplying) { return; }

        var id = idMap.get(el);
        emit({ type: 'mutate', field_id: id, value: valueOf(el, kind) });
    }, true);

    // ---- browser → renderer IPC (Stage 2) ------------------------------

    window.__buffrEditApply = function (fieldId, newValue) {
        var el = elFor(fieldId);
        if (!el) { return false; }
        var kind = kindOf(el);
        if (!kind) { return false; }
        // Mark our own write so the input listener ignores it.
        el.__buffrApplying = true;
        try {
            if (kind === 'input' || kind === 'textarea') {
                if (el.value !== newValue) { el.value = newValue; }
            } else if (kind === 'contentEditable') {
                if (el.innerText !== newValue) { el.innerText = newValue; }
            }
            // Fire input event so site JS bound to the field stays in sync.
            el.dispatchEvent(new Event('input', { bubbles: true }));
        } finally {
            el.__buffrApplying = false;
        }
        return true;
    };

    window.__buffrEditAttach = function (fieldId) {
        var el = elFor(fieldId);
        if (!el) { return false; }
        el.classList.add(OVERLAY_CLASS);
        return true;
    };

    window.__buffrEditDetach = function (fieldId) {
        var el = elFor(fieldId);
        if (!el) { return false; }
        el.classList.remove(OVERLAY_CLASS);
        return true;
    };

    // Cycle focus among visible editable text fields. Insert mode's
    // Tab/Shift+Tab is intercepted by the apps layer and routed here
    // so navigation skips links/buttons and only lands on inputs.
    //
    // "Visible" mirrors focus_first_input.js: non-zero rect, not
    // display:none, not visibility:hidden. Wraps at both ends.
    window.__buffrCycleInput = function (forward) {
        var sel = 'input:not([type=hidden]):not([disabled]):not([readonly]),'
            + 'textarea:not([disabled]):not([readonly]),'
            + '[contenteditable="true"]';
        function visible(el) {
            if (!el) return false;
            var r = el.getBoundingClientRect();
            if (r.width <= 0 || r.height <= 0) return false;
            var s = getComputedStyle(el);
            if (s.visibility === 'hidden' || s.display === 'none') return false;
            return true;
        }
        var nodes = [];
        var all = document.querySelectorAll(sel);
        for (var i = 0; i < all.length; i++) {
            if (visible(all[i])) { nodes.push(all[i]); }
        }
        if (nodes.length === 0) { return; }
        var cur = document.activeElement;
        var idx = nodes.indexOf(cur);
        var nextIdx;
        if (idx === -1) {
            nextIdx = forward ? 0 : nodes.length - 1;
        } else {
            nextIdx = forward
                ? (idx + 1) % nodes.length
                : (idx - 1 + nodes.length) % nodes.length;
        }
        var target = nodes[nextIdx];
        target.focus();
        target.scrollIntoView({ block: 'center' });
        target.dispatchEvent(new FocusEvent('focusin', { bubbles: true }));
    };

    // Re-focus a previously-focused field by its buffr-assigned ID.
    // Called by Rust when the user presses `i` and a last-focused ID
    // is known.  Falls through to the page's own focusin handling;
    // edit.js will fire a Focus event back through the console bridge.
    window.__buffrEditFocus = function (id) {
        var el = elFor(id);
        if (!el) { return; }
        el.focus();
        el.scrollIntoView({ block: 'center' });
        el.dispatchEvent(new FocusEvent('focusin', { bubbles: true }));
    };

})();
