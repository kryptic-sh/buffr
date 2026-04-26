# Hint mode — DOM-injected overlay labels

Phase 3 of `PLAN.md` ships Vimium-style follow-by-letter-label hints: press `f`
to enter hint mode, type a few letters, the matched element gets clicked. `F` is
the background-tab variant — single-tab buffr falls back to a same-tab click
with a `tracing::warn!` breadcrumb until the tab strip lands.

## Architecture: DOM injection

Hints render as real `<div class="buffr-hint-overlay">` elements appended to the
page DOM. The host injects `crates/buffr-core/assets/hint.js` via
`cef::Frame::execute_java_script` after substituting three placeholders
(`__ALPHABET__`, `__LABELS__`, `__SELECTORS__`). The JS enumerates visible
matching elements, assigns sequential `data-buffr-hint-id` attributes, and
renders an overlay div per target.

This sidesteps the cross-process compositor work the OSR + wgpu path would have
required. `docs/ui-stack.md` records that compositing overlays on top of CEF's
surface is the trigger to migrate the chrome layer to OSR; we deferred that by
punting the rendering into the page itself instead.

## IPC: console-log scraping (chosen)

CEF -> Rust uses the **console-log fallback** path, not `cef_process_message_t`.
The injected JS calls

    console.log("__buffr_hint__:" + JSON.stringify(payload))

and `BuffrDisplayHandler::on_console_message` (in
`crates/buffr-core/src/handlers.rs`) pattern-matches the sentinel, parses the
JSON tail with `serde_json`, and writes into a one-slot `HintEventSink`
(`Arc<Mutex<Option<HintConsoleEvent>>>`). The host drains the sink each tick
from `BrowserHost::pump_hint_events`.

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

- `__buffrHintFilter(typed)` — dim every overlay whose label doesn't start with
  `typed`.
- `__buffrHintCommit(elementId)` — focus + click the element with the matching
  `data-buffr-hint-target-id`, then call `__buffrHintCancel()` to clean up.
- `__buffrHintCancel()` — remove every injected overlay div, strip every
  `data-buffr-hint-target-id` attribute, and null out the three globals.

## CSS

Every overlay carries the class `buffr-hint-overlay`. The injected
`<style id="buffr-hint-style">` tag pins:

- `position: fixed`
- `z-index: 2147483647` (`HINT_OVERLAY_Z_INDEX`, max int32 — page stacking
  contexts can't shadow the hints)
- vivid yellow background (`#FFD83A`), dark text, monospace 11px
- `pointer-events: none` so the page below stays interactive
- additional `buffr-hint-typed` (dimmed) and `buffr-hint-hidden` (display:none)
  classes the filter callback toggles

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
rejects empty / non-ASCII / duplicate inputs at config-load time so the runtime
path never has to handle them.
