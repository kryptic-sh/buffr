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

## Decision — Option C with `wgpu` OSR on Linux and macOS

CEF paints the page into an off-screen buffer, then the app composites that
buffer plus the tab strip, overlays, and statusline into the same `winit`
window with `wgpu`. Windows still uses the native child-window path for now.

Linux needs OSR because X11/XWayland child-window embedding is not supported.
macOS also uses OSR because AppKit child views do not layer predictably with
buffr's custom chrome: the native CEF child can cover the tabbar/statusline or
land at a different origin than the chrome compositor.

### Why OSR wins now

- One `winit` window — no inter-window placement bugs.
- Page and chrome share one coordinate system.
- Hints, command overlays, tabbar, statusline, and page content can be composed
  in z-order by the renderer.
- CEF child-view geometry does not need platform-specific AppKit/X11 resizing.

### Windowed Exception

Windows maps `RawWindowHandle::Win32(_)` to `HostMode::Windowed`. That path
parents CEF as a native child window and calls `was_resized()` after winit
resize events.

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
- CEF page rect:
  `(0, overlay_h + TAB_STRIP_HEIGHT, w, h - overlay_h - TAB_STRIP_HEIGHT - STATUSLINE_HEIGHT)`,
  where `overlay_h` is `INPUT_HEIGHT + dropdown_rows * STATUSLINE_HEIGHT` when
  an overlay is open, `0` otherwise. In OSR mode this rect becomes the CEF
  `view_rect` and the renderer composites the painted buffer at the same
  position. Whenever overlays open or close, the app re-issues the resize so
  CEF re-flows the page area.
- Renderer surface: a single `wgpu` surface sized to the full window. Each frame
  composites the page, tab strip, statusline, overlays, hints, and popups in one
  pass.
