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

## Decision — Option C, `wgpu` OSR on every platform

CEF paints the page into an off-screen buffer, then the app composites that
buffer plus the tab strip, overlays, and statusline into the same `winit` window
with `wgpu`. This is now the path on **all** platforms — `windowless_rendering`
is enabled unconditionally and there is no native child-window mode left in the
tree.

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

### Historical: the windowed exception

Earlier revisions mapped `RawWindowHandle::Win32(_)` onto a `HostMode::Windowed`
path that parented CEF as a native child window. That mode has been removed —
`HostMode` no longer has a windowed variant and Windows composites through OSR
like everything else.

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
  position. Whenever overlays open or close, the app re-issues the resize so CEF
  re-flows the page area.
- Renderer surface: a single `wgpu` surface sized to the full window. Each frame
  composites the page, tab strip, statusline, overlays, hints, and popups in one
  pass.

## Update — 2026-05-03

**Dedicated `wgpu-render` worker thread** (v0.3.0).

All `wgpu` mutating calls now run on a dedicated `wgpu-render` worker thread.
The UI thread does only:

1. `surface.get_current_texture()` — acquire the swapchain image.
2. Chrome paint closure (CPU-only rasterize of tab strip / statusline).
3. OSR pixel `memcpy` into a staging buffer.
4. `try_send(RenderCommand)` over a capacity-1 mailbox channel.

The worker thread handles `queue.write_texture`, `queue.submit`, and
`surface_texture.present()`. This decouples the UI event loop from Wayland
compositor backpressure: `present()` was previously blocking the main thread for
multiple seconds on Hyprland workspace switches, causing perceived freezes.

Additional constraints introduced alongside the worker:

- `frames_in_flight` counter gates `get_current_texture()` — only one acquired
  `SurfaceTexture` outstanding at a time (`desired_maximum_frame_latency = 1`).
- `Renderer::drop` wraps `surface` and `device` in `ManuallyDrop`; when the
  worker is mid-`present()` during shutdown, the wgpu state is leaked rather
  than triggering a "Surface in use" panic.
- `resize()` defers `surface.configure()` into a `pending_resize` slot when the
  worker holds an outstanding `SurfaceTexture`; the next `frame()` applies it
  once the worker drains.
