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
// `%%SENTINEL%%` is substituted by `edit::build_inject_script` with
// `__buffr_edit__:` + the per-page-load nonce + `:`. Rust rejects any line
// whose nonce isn't the one it minted for this browser, which is what stops
// a third-party iframe from forging edit events (notably
// `{"type":"selection"}`, which feeds the yank-to-clipboard path). Keep it
// in this IIFE's scope — never assign it to `window`.
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
//
// The guard alone is not enough now that the sentinel carries a per-load
// nonce: a re-injection that bailed on the guard would leave the *already
// wired* closure emitting the previous nonce, and Rust would drop every
// event from then on. So a re-injection tears the previous copy down via
// `window.__buffrEditTeardown()` and wires itself fresh.
//
// Teardown deliberately takes no arguments and returns nothing. A page can
// squat the name to make its own edit mode stop working — which it could
// already do by setting `__buffrEditWired` — but there is no path here that
// hands page-controlled code the nonce.

(function () {
    'use strict';

    // A previous copy of this script is live in this same document (the
    // guard survived because `window` did). Unwire it so this copy, with
    // the current nonce, fully replaces it.
    if (typeof window.__buffrEditTeardown === 'function') {
        try { window.__buffrEditTeardown(); } catch (_) {}
    }
    // Still wired means something else owns the flag (a page squatting the
    // name). Bail rather than stack a second listener set on top.
    if (window.__buffrEditWired) { return; }
    window.__buffrEditWired = true;

    var SENTINEL = '%%SENTINEL%%';
    var OVERLAY_CLASS = '%%OVERLAY_CLASS%%';

    // User-gesture gate. Stays false until the user actually interacts
    // with the page (mouse / touch / pen). The Rust side flips this to
    // true via run_js before deliberate keyboard-driven focuses (i,
    // FocusFirstInput, etc.). Until it flips, every DOM focus is
    // immediately blurred — pages that autofocus inputs on load
    // (or post-load via setTimeout / rAF / observers) would otherwise
    // keep the caret blinking, which forces continuous on_paint frames
    // from CEF and pegs buffr's render pipeline at idle.
    //
    // Preserved across a re-wire: the user's gesture on this document
    // already happened, and re-arming the gate would silently blur the
    // field they are typing in.
    if (window.__buffrUserGesture !== true) { window.__buffrUserGesture = false; }
    function markGesture() { window.__buffrUserGesture = true; }
    document.addEventListener('mousedown', markGesture, true);
    document.addEventListener('pointerdown', markGesture, true);
    document.addEventListener('touchstart', markGesture, true);

    // A field that is already focused when this script installs — the
    // classic case being `autofocus`, which fires before injection — never
    // produces a `focusin` we can see. Report it once at install time so it
    // is not silently unreachable.
    //
    // This used to blur it instead, on the reasoning that buffr starts in
    // Normal mode. But the page had put the caret in a text field and the
    // user could see it there, while every keystroke went to the keymap
    // instead: focused to look at, dead to type in.
    function reportExistingFocus() {
        var el = document.activeElement;
        if (!el) { return; }
        // delegatesFocus puts the HOST in activeElement while the caret sits
        // on an inner node; walk in to find who really has it.
        while (el.shadowRoot && el.shadowRoot.activeElement) {
            el = el.shadowRoot.activeElement;
        }
        var kind = kindOf(el);
        if (!kind) { return; }
        var id = idFor(el);
        if (id === lastFocusId) { return; }
        lastFocusId = id;
        el.classList.add(OVERLAY_CLASS);
        emit({
            type: 'focus',
            field_id: id,
            kind: kind,
            value: valueOf(el, kind),
            selection_start: (kind !== 'contentEditable') ? el.selectionStart : null,
            selection_end: (kind !== 'contentEditable') ? el.selectionEnd : null
        });
    }
    // Invoked at the very bottom of this IIFE, not here: it calls idFor(),
    // and `var idMap` below is hoisted as undefined until its assignment
    // runs. Calling it at this point threw, which aborted the whole IIFE
    // and left the listeners uninstalled — edit mode dead on any page that
    // already had a focused field.
    if (document.readyState !== 'complete') {
        window.addEventListener('load', reportExistingFocus, { once: true });
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

    // `<input>` covers far more than text entry. Entering Insert on a
    // checkbox or a range slider traps the user: Normal-mode keys stop
    // reaching the keymap while the field cannot accept text anyway.
    // Allow-list by exclusion — unknown/new types default to editable,
    // which fails toward "the user can type", the safer direction.
    var NON_EDITABLE_INPUT_TYPES = {
        button: 1, checkbox: 1, color: 1, file: 1, hidden: 1, image: 1,
        radio: 1, range: 1, reset: 1, submit: 1
    };

    function kindOf(el) {
        if (!el || !el.tagName) { return null; }
        var tag = el.tagName.toUpperCase();
        if (tag === 'TEXTAREA') { return 'textarea'; }
        if (tag === 'INPUT') {
            // `.type` normalises to lowercase and falls back to "text" for
            // an absent or unrecognised attribute, matching the platform.
            var t = (el.type || 'text').toLowerCase();
            if (NON_EDITABLE_INPUT_TYPES[t] === 1) { return null; }
            return 'input';
        }
        if (el.isContentEditable) { return 'contentEditable'; }
        // designMode: the whole document is editable and no element carries
        // contenteditable, so focus lands on <body> with isContentEditable
        // false. `designMode` is the only signal.
        try {
            var doc = el.ownerDocument;
            if (doc && doc.designMode === 'on'
                && (tag === 'BODY' || tag === 'HTML')) {
                return 'contentEditable';
            }
        } catch (_) {}
        return null;
    }

    // The element that actually holds the caret.
    //
    // `focusin` is retargeted to the shadow HOST when focus crosses a shadow
    // boundary, so `ev.target` on a custom element is <my-field>, not the
    // <input> inside it — and kindOf() on the host returns null, so the
    // event was dropped and Insert never engaged. composedPath()[0] is the
    // real focused node, and it works for open, closed, nested and
    // delegatesFocus roots alike because the path is built by the platform.
    function deepTarget(ev) {
        var path = (ev && ev.composedPath) ? ev.composedPath() : null;
        if (path && path.length) { return path[0]; }
        return ev ? ev.target : null;
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

    // Last element reported as focused. `focusin` and `focus` both fire for
    // the same change, and reportExistingFocus can race either, so without
    // this the browser would see two or three Focus events per real focus.
    var lastFocusId = null;

    function onFocusIn(ev) {
        var el = deepTarget(ev);
        var kind = kindOf(el);
        if (!kind) { return; }
        var seen = idFor(el);
        if (seen === lastFocusId) { return; }
        lastFocusId = seen;

        // Every focus of an editable field is reported, including ones the
        // page drove itself (autofocus, .focus() after a fetch, a dialog
        // grabbing its search box). Focus is focus: if the caret is in a
        // text field, typing must reach it. This used to re-blur anything
        // without a preceding user gesture, which is why autofocused
        // fields looked focused but silently swallowed every keystroke.
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
    }
    document.addEventListener('focusin', onFocusIn, true);
    // Fallback for pages that swallow `focusin`. A capture listener on
    // `window` runs before one on `document`, so a page that calls
    // stopPropagation there hides the event from us entirely — and ours
    // cannot be registered first, because this script is injected after the
    // page's own scripts have run.
    //
    // `focus` is a separate event: it does not bubble, but capture still
    // walks down to the target, and stopping `focusin` does nothing to it.
    // onFocusIn is idempotent per element (see lastFocusId), so a page that
    // stops neither simply reports once.
    document.addEventListener('focus', onFocusIn, true);

    // ---- focusout (capture) ---------------------------------------------
    //
    // Fires when any element loses focus. We remove the overlay class and
    // emit a blur event so Stage 2 can drop the EditSession.

    function onFocusOut(ev) {
        var el = deepTarget(ev);
        var kind = kindOf(el);
        if (!kind) { return; }

        var id = idFor(el);
        el.classList.remove(OVERLAY_CLASS);
        // Clear the latch so re-focusing this same field reports again.
        if (lastFocusId === id) { lastFocusId = null; }

        emit({ type: 'blur', field_id: id });
    }
    document.addEventListener('focusout', onFocusOut, true);

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

    function onInput(ev) {
        var el = deepTarget(ev);
        var kind = kindOf(el);
        if (!kind) { return; }

        // Only forward events for fields that already have a buffr ID.
        // `idMap.has` is a WeakMap lookup — O(1), no allocation.
        if (!idMap.has(el)) { return; }

        // Skip echoes of our own DOM writes.
        if (el.__buffrApplying) { return; }

        var id = idMap.get(el);
        emit({ type: 'mutate', field_id: id, value: valueOf(el, kind) });
    }
    document.addEventListener('input', onInput, true);

    // ---- teardown -------------------------------------------------------
    //
    // Called by a *later* injection of this same script into this same
    // document (see the guard at the top). Removes every listener this
    // copy installed and clears the wired flag so the new copy — carrying
    // the current console nonce — can install cleanly. Takes no arguments
    // and returns nothing, so a page that squats the name learns nothing.

    window.__buffrEditTeardown = function () {
        document.removeEventListener('focusin', onFocusIn, true);
        document.removeEventListener('focus', onFocusIn, true);
        document.removeEventListener('focusout', onFocusOut, true);
        document.removeEventListener('input', onInput, true);
        document.removeEventListener('mousedown', markGesture, true);
        document.removeEventListener('pointerdown', markGesture, true);
        document.removeEventListener('touchstart', markGesture, true);
        window.__buffrEditWired = false;
    };

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

    // Snapshot the current document selection and ship it to Rust via
    // the console-log sentinel. Called from the YankSelection arm so
    // the Rust side can write the text into the system clipboard
    // through hjkl-clipboard, sidestepping Chromium's editor copy
    // command (which on some Wayland builds writes only to its own
    // internal clipboard rather than wl_data_device).
    window.__buffrEmitSelection = function () {
        var s = '';
        try { s = window.getSelection ? String(window.getSelection() || '') : ''; } catch (_) {}
        emit({ type: 'selection', value: s });
    };

    // Everything above is declared; safe to inspect the existing focus now.
    // `autofocus` fires before this script is injected and produces no
    // focusin we could observe, so without this the field the page focused
    // on load would be typed into by nobody.
    reportExistingFocus();

})();
