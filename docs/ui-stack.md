# UI stack — chrome rendering decision

Phase 3 of `PLAN.md` introduces native chrome (statusline, tab strip, command
line, hint overlay). This ADR records the rendering stack chosen for the first
batch of chrome — statusline today, tab strip + command line later in Phase 3.

## Options

- **A — `softbuffer` strip in the same `winit` window.** Chrome lives in a
  CPU-blitted strip docked to the bottom (or top) of the buffr window. CEF's
  child window is sized to the remaining rectangle and reparented through
  `WindowInfo::parent_window`. One window, no compositor placement, no GPU
  dependency.
- **B — separate top-level `winit` windows for chrome.** Each chrome panel is
  its own OS window positioned over the CEF window. Avoids resizing CEF, but
  Linux compositors (especially Wayland) routinely refuse client-requested
  positioning and z-ordering. Fragile.
- **C — OSR + `wgpu` compositor.** CEF paints into a buffer via
  `CefRenderHandler::OnPaint`; chrome is drawn as `wgpu` quads on top. Required
  for hint mode (per-pixel composition over the live page) and native Wayland.
  Pulls in `wgpu`, `naga`, shaders, plus the OSR plumbing the `osr` feature
  already scaffolds.

## Decision — Option A with `softbuffer = "0.4"`

`softbuffer` is small, depends only on platform window-handle crates, and a
single-line statusline rendered with a bundled bitmap font is trivial to
software-blit. The current CEF embedding is windowed (X11/XWayland on Linux),
which already requires us to give CEF its own subrectangle inside the winit
window — A composes naturally with that. C drags in a GPU stack we do not
otherwise need at this phase.

### Why A wins now

- One `winit` window — no inter-window placement bugs.
- No `wgpu` dependency for a 24-px strip.
- CEF's windowed embedding stays in charge of page rendering.
- Future tab strip and command line slot into the same `softbuffer::Surface`.

### Trigger to migrate to C

Hint mode requires drawing labelled overlays _on top of_ the live page, anchored
to DOM rectangles, and updating at scroll/animation rates. That is per-pixel
compositing, which a CPU strip cannot do without flicker or expensive readback.
When hint mode lands (later in Phase 3), the chrome layer migrates to the OSR +
`wgpu` path that `crates/buffr-core/src/osr.rs` already scaffolds. Statusline
and tab strip may stay on A or be ported in the same change — whichever costs
less.

## Layout

- `STATUSLINE_HEIGHT = 24` pixels, docked to the bottom of the buffr window.
- `TAB_STRIP_HEIGHT = 30` pixels, sits above the CEF page area and below the
  optional input bar. Always painted (zero tabs renders an empty bar in the
  strip's bg colour).
- `INPUT_HEIGHT = 28` pixels, docked to the **top** when the command line or
  omnibar is open. The input strip is hidden when the overlay is closed and the
  page region reclaims those rows.
- Suggestion dropdown: each row is `STATUSLINE_HEIGHT` (24 px) tall, max 8 rows.
  Stacks below the input strip when populated; the dropdown rectangle also
  shrinks the CEF child rect so suggestions never overlap the page.
- CEF child window rect:
  `(0, overlay_h + TAB_STRIP_HEIGHT, w, h - overlay_h - TAB_STRIP_HEIGHT - STATUSLINE_HEIGHT)`,
  where `overlay_h` is `INPUT_HEIGHT + dropdown_rows * STATUSLINE_HEIGHT` when
  an overlay is open, `0` otherwise. The X11 XID is passed as
  `WindowInfo::parent_window` at creation time; on resize the host walks every
  tab and calls `cef::Browser::host().was_resized()` after winit reports the new
  size. Whenever the overlay opens / closes we re-issue the resize so CEF
  re-flows the page area.
- Statusline + input paint surface: a single `softbuffer::Surface` sized to the
  full window. Each frame we paint the bottom strip (statusline) and, when an
  overlay is active, the top strip (input bar + dropdown). The middle page
  region is owned by CEF and never touched by us; `present_with_damage` is
  called with up to two damage rects so the page area is never repainted by
  softbuffer.
