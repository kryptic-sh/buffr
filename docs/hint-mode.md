# Hint mode — DOM-injected overlay labels

Vimium-style follow-by-letter-label hints: press `f` to enter hint mode, type a
few letters, the matched element gets clicked. `F` is nominally the
background-tab variant, but it still commits as a same-tab click and only logs a
`tracing::warn!` breadcrumb; routing the commit through `open_tab_background` is
**not implemented**.

## Architecture: DOM injection

Hints render as real `<div class="buffr-hint-overlay">` elements appended to the
page DOM. The host injects `crates/buffr-core/assets/hint.js` via
`cef::Frame::execute_java_script` after substituting four placeholders
(`__ALPHABET__`, `__LABELS__`, `__SELECTORS__`, and `%%SENTINEL%%` — the
per-session nonce, see [The nonce](#the-nonce)). The JS enumerates visible
matching elements, assigns sequential `data-buffr-hint-id` attributes, and
renders an overlay div per target.

This sidesteps compositing the labels ourselves. The chrome layer has since
moved to OSR + `wgpu` (see [`ui-stack.md`](./ui-stack.md)), but the hints stayed
in the page DOM: they need per-element geometry that the renderer already knows,
and keeping them there costs no extra compositor work.

## IPC: console-log scraping (chosen)

CEF -> Rust uses the **console-log fallback** path, not `cef_process_message_t`.
The injected JS calls

    console.log("__buffr_hint__:" + nonce + ":" + JSON.stringify(payload))

and `BuffrDisplayHandler::on_console_message` (in
`crates/buffr-cef/src/handlers.rs`) pattern-matches the sentinel via the shared
`buffr_core::console_sentinel` helper, parses the JSON tail with `serde_json`,
and writes into a one-slot `HintEventSink`
(`Arc<Mutex<Option<HintConsoleEvent>>>`). The host drains the sink each tick
from `BrowserHost::pump_hint_events`.

### The nonce

`on_console_message` has no frame argument, so without authentication _any_
frame — including a third-party ad iframe — could emit a sentinel line and have
it accepted. A page doing that could point the next hint keystroke at an element
it chose, pin the idle inhibitor on so the screen never locks, or push text into
the yank-to-clipboard path.

So every sentinel line carries a 128-bit nonce (`buffr_core::console_nonce`),
minted from the OS CSPRNG and spliced into the injected script:

    <sentinel><nonce>:<json>

The page nonce rotates on every main-frame load and the hint nonce on every
`enter_hint_mode`. Nonces only ever reach main frames, so a subframe can never
learn one. The match is also **anchored** at the start of the console line
rather than located with `find` anywhere in it.

Two consequences worth knowing:

- **This is not a boundary against the top frame.** The injected script runs in
  the page, and injection happens at `on_load_end` — after page script has run —
  so a page that hooks `console.log` first reads the nonce and can forge for
  itself. What the nonce closes is cross-frame forgery and cross-load replay.
  The complete fix is a real `cef_process_message_t` channel; this is defence in
  depth on a transport that is structurally observable.
- **Anchoring is an availability trade.** A page that wraps `console.log` to
  prepend its own format string (`%cINFO …`) now hides our payload too, so hint
  and edit mode stop working on it. Accepted deliberately: on such a page the
  nonce is readable anyway, so the alternative is a channel the page controls.

The cleaner `cef_process_message_t` IPC channel was rejected for v1 because it
requires a renderer-side `RenderProcessHandler` registered via
`CefApp::on_render_process_handler`, plus a V8 binding so JS can call
`frame->SendProcessMessage(PID_BROWSER, msg)`. That's helper-subprocess plumbing
for a single one-way "hint list" message. Console-log scraping reuses the
display handler we already wired and works identically end-to-end. If the hint
list ever needs to flow at animation rates (live scroll-position updates), we'll
revisit.

Rust -> CEF stays on `execute_java_script`: the host calls
`window.__buffrHintFilter(typed)`, `__buffrHintCommit(id)`, or
`__buffrHintCancel()` from `BrowserHost::feed_hint_key` / `backspace_hint` /
`cancel_hint`.

## JS surface

The injected script exposes three globals on `window`:

- `__buffrHintFilter(typed)` — **hide** every overlay whose label doesn't start
  with `typed` (via the `buffr-hint-hidden` class, `display: none`). Overlays
  that still match are re-shown, with the already-typed prefix wrapped in a
  `<span class="buffr-hint-typed-prefix">` so the user sees how far they've
  narrowed the label.
- `__buffrHintCommit(elementId)` — focus + click the element with the matching
  `data-buffr-hint-target-id`, then call `__buffrHintCancel()` to clean up.
- `__buffrHintCancel()` — remove every injected overlay div, strip every
  `data-buffr-hint-target-id` attribute, and null out the three globals.

## CSS

Every overlay carries the class `buffr-hint-overlay`. The injected
`<style id="buffr-hint-style">` tag pins:

- `position: fixed`
- `z-index: 2147483647` (max int32 — page stacking contexts can't shadow the
  hints); the literal lives in `crates/buffr-core/assets/hint.js`
- vivid yellow background (`#FFD83A`), black text, a `#C8AA10` border, and
  `font: bold 11px/1.4 -apple-system,BlinkMacSystemFont,"Segoe UI",monospace` —
  so on mainstream platforms the label renders in the system UI font, with
  `monospace` only as a last-resort fallback
- `pointer-events: none` so the page below stays interactive
- `.buffr-hint-overlay.buffr-hint-hidden { display: none !important }` — applied
  by the filter callback to non-matching overlays
- `.buffr-hint-overlay .buffr-hint-typed-prefix { opacity: .45; text-decoration: line-through }`
  — applied to the child span holding the already-typed prefix of a
  still-matching label

## Label algorithm

`HintAlphabet::labels_for(count)` is a port of Vimium's hud.js BFS:

1. Empty-string seed in a queue, walked breadth-first.
2. Each pop expands by every alphabet char (prepended).
3. Stop once the unexpanded slice (`queue[offset..]`) holds enough.
4. Reverse each entry, then sort by alphabet position.

This guarantees uniqueness, no-prefix-collisions, and that the first N
enumerated elements get the shortest labels.

## Config

`[hint] alphabet = "asdfghjkl;weruio"` controls the character set. Validation
rejects empty, non-ASCII, duplicate-bearing, and shorter-than-two-character
inputs at config-load time, so the runtime path never has to handle them.
