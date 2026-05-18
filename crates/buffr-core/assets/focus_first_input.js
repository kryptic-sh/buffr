// Focus the first VISIBLE editable text field on the page.
//
// Triggered by `i` / `gi` chords. If an editable element is already
// focused, refire focusin so edit.js posts a fresh event regardless
// (DOM doesn't fire focusin when focus() is called on the already-
// focused element).
//
// "Editable" = INPUT (excluding hidden/disabled/readonly), TEXTAREA
// (excluding disabled/readonly), or [contenteditable="true"].
//
// "Visible" = non-zero bounding rect, not display:none, not
// visibility:hidden. Sites like duckduckgo.com place hidden inputs
// early in the DOM that we must skip.
(function () {
    var sel = 'input:not([type=hidden]):not([disabled]):not([readonly]),'
        + 'textarea:not([disabled]):not([readonly]),'
        + '[contenteditable="true"]';
    function editable(el) {
        if (!el) return false;
        var t = (el.tagName || '').toUpperCase();
        return t === 'INPUT' || t === 'TEXTAREA' || el.isContentEditable;
    }
    function visible(el) {
        if (!el) return false;
        var r = el.getBoundingClientRect();
        if (r.width <= 0 || r.height <= 0) return false;
        var s = getComputedStyle(el);
        if (s.visibility === 'hidden' || s.display === 'none') return false;
        return true;
    }
    var cur = document.activeElement;
    if (editable(cur) && visible(cur)) {
        cur.dispatchEvent(new FocusEvent('focusin', { bubbles: true }));
        return;
    }
    var nodes = document.querySelectorAll(sel);
    for (var i = 0; i < nodes.length; i++) {
        if (visible(nodes[i])) {
            nodes[i].focus();
            nodes[i].scrollIntoView({ block: 'center' });
            nodes[i].dispatchEvent(new FocusEvent('focusin', { bubbles: true }));
            return;
        }
    }
})();
