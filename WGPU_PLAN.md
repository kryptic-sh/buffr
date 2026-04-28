# Plan: wgpu base renderer + CEF GPU acceleration + CI concurrency

Drafted 2026-04-29 by Opus on `main`. Implementation by sonnet sub-agent.
Companion to `PLAN.md` (the long-term roadmap); this file scopes one branch of
work.

## Goal

Four deliverables in one branch:

1. **Replace softbuffer with wgpu** as the window's base presentation layer.
   Fixes the Wayland resize stretch (chrome strip lags one frame behind
   compositor configure when softbuffer's CPU buffer-swap races the
   configure-ack). Cross-platform — same code path on Wayland / X11 / macOS /
   Windows.

2. **Enable CEF GPU acceleration** for the OSR pipeline. Today `chrome://gpu`
   reports software compositing for canvas / WebGL / video decode. Add the
   Chromium switches needed to flip those to "Hardware accelerated". (Zero-copy
   shared-texture OSR is OUT of scope — that is a follow-up.)

3. **Linux backend selection** — auto-pick Wayland on Wayland, X11 on X11; add a
   `--x11` CLI flag (Linux only) to force the X11 backend on a Wayland session
   for testing. macOS / Windows: native, no flag, no branching.

4. **CI concurrency** — cancel in-progress runs when a new run lands on the same
   workflow + ref.

## Why

- softbuffer can't keep up with Wayland's configure-ack handshake during
  interactive resize. The chrome strip (tab strip, statusline, prompts) renders
  at the previous dims while the compositor stretches the old wl_buffer to the
  new surface size. Verified visually 2026-04-29 in `~/Pictures/temp.png` (tab +
  statusline narrow at left while OSR fills the new wide area).
- wgpu's `Surface::configure` is the canonical winit-paired present layer. It
  commits buffer dims atomically with each frame, so configure-ack races
  disappear.
- Same `wgpu::Surface` API on Vulkan (Linux primary) / GL (Linux fallback) /
  Metal (macOS) / DX12 (Windows). No platform branching.
- CEF's OSR pipeline currently runs the page compositor in software even though
  the host has a working GPU. WebGL, canvas, video decode all suffer. The fix is
  a handful of Chromium command-line switches — no API changes.

## Non-goals (this branch)

- Rewriting `crates/buffr-ui` to use wgpu draw calls. The chrome widgets keep
  their CPU rasterizer (`&mut [u32]` row-major). We only swap the present layer
  in `apps/buffr`.
- Shared-texture OSR (`accelerated_paint`, dmabuf / DXGI / IOSurface zero-copy).
  Big platform-specific work; track separately.
- Replacing `HostMode::Windowed` (X11 native child window for CEF on Linux X11).
  That path stays as-is for now.
- Migrating other binaries (hjkl, etc.) — buffr only.

## Architecture

### Today

```text
paint_chrome_with(new_size):
    softbuffer::Surface::resize(w, h)
    let mut buf = surface.buffer_mut()        // &mut [u32], row-major
    statusline.paint(buf, w, h)
    tab_strip.paint(buf, w, h, tab_y)
    overlay.paint_at(...)                     // omnibar popup
    /* OSR scale loop: BGRA -> buf u32 in browser rect */
    buf.present_with_damage(rects)
```

### After

```text
paint_chrome_with(new_size):
    if renderer.dims != (w, h): renderer.reconfigure(w, h)
    renderer.frame(|cpu_buf, w, h| {
        statusline.paint(cpu_buf, w, h)
        tab_strip.paint(cpu_buf, w, h, tab_y)
        overlay.paint_at(...)
        /* same OSR scale loop, unchanged */
    })
    // renderer.frame uploads cpu_buf to a single GPU texture and
    // draws it as a fullscreen triangle, then presents.
```

Single texture, single fullscreen-triangle pipeline. Chrome paint code and OSR
scale loop are byte-for-byte unchanged. Only the present layer swaps.

The damage-rect bookkeeping (`Vec<softbuffer::Rect>`) is dropped — wgpu always
presents the full surface and the compositor handles damage tracking.

## Implementation steps

### Step 1 — dependencies (`apps/buffr/Cargo.toml`)

Remove:

```toml
softbuffer = "..."
```

Add:

```toml
wgpu = "22"        # current latest stable; check crates.io and pin
pollster = "0.3"   # block-on for adapter request
bytemuck = { version = "1", features = ["derive"] }
```

`wgpu` is the only large new dep. `pollster` is small (block_on for the async
`request_adapter` / `request_device`). `bytemuck` is for the texture upload
bytes view.

Re-run `cargo generate-lockfile` after the swap (per BCTP rules in CLAUDE.md).
`Cargo.lock` MUST be committed (per the saved "Cargo.lock must be tracked"
memory).

### Step 2 — new module `apps/buffr/src/render.rs`

```rust
//! wgpu-based present layer. Replaces softbuffer.
//!
//! Architecture: chrome and OSR composite are painted into a single
//! CPU buffer (`Vec<u32>` matching today's softbuffer u32 layout),
//! uploaded to one GPU texture, drawn as a fullscreen triangle, then
//! presented.
//!
//! Why a single texture instead of separate chrome + OSR quads:
//! preserves the existing CPU paint pipeline byte-for-byte. The
//! chrome widgets in `crates/buffr-ui` already write into a row-major
//! `&mut [u32]`; swapping the present layer is the minimal change.

use std::sync::Arc;
use winit::window::Window;

pub struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    sampler: wgpu::Sampler,
    bind_group_layout: wgpu::BindGroupLayout,
    bind_group: wgpu::BindGroup,
    cpu_buf: Vec<u32>,
    width: u32,
    height: u32,
}

impl Renderer {
    /// Initialise wgpu against `window`. Blocks on adapter / device.
    pub fn new(window: Arc<Window>) -> anyhow::Result<Self> { /* ... */ }

    /// Reconfigure the surface + reallocate texture to the new size.
    /// Idempotent if dims unchanged.
    pub fn resize(&mut self, w: u32, h: u32) { /* ... */ }

    /// Paint into the CPU buffer via `paint`, upload to the texture,
    /// draw, present. `paint` receives `(buf, width, height)` exactly
    /// as the previous softbuffer flow handed it.
    pub fn frame<F>(&mut self, paint: F) -> anyhow::Result<()>
    where
        F: FnOnce(&mut [u32], usize, usize),
    { /* ... */ }
}
```

Implementation notes:

- **Texture format**: `Bgra8Unorm`. The CPU buffer is u32 with
  `(r << 16) | (g << 8) | b` packing. On little-endian, that u32 lays out as
  `[B, G, R, 0x00]` in memory, which is BGRA. Linear, not sRGB — chrome paints
  are already in display space.
- **Surface format**: prefer `Bgra8Unorm` to match the texture. Fall back to
  whatever the surface advertises if not supported, and convert in the fragment
  shader if needed.
- **Pipeline**: vertex stage emits a fullscreen triangle from `vertex_index` (no
  vertex buffer). Fragment stage samples the texture with a linear sampler. WGSL
  inline.
- **Present mode**: `Fifo` (vsync). `Mailbox` only as a future toggle.
- **Backends**: `wgpu::Backends::all()`. Let wgpu pick — Vulkan on Linux, Metal
  on macOS, DX12 on Windows.
- **Adapter**: `power_preference: HighPerformance`. Request adapter with
  `compatible_surface: Some(&surface)`.
- **Error path**: propagate via `anyhow::Result`; caller (`resumed`) logs and
  exits if init fails.

WGSL (inline string):

```wgsl
struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) vi: u32) -> VsOut {
    // Fullscreen triangle covering [-1,1]^2 with UVs in [0,1].
    var pos = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 3.0, -1.0),
        vec2<f32>(-1.0,  3.0),
    );
    // UV (0,0) is top-left of the texture for our convention; the
    // chrome buffer's row 0 is the top of the window.
    var uv = array<vec2<f32>, 3>(
        vec2<f32>(0.0, 1.0),
        vec2<f32>(2.0, 1.0),
        vec2<f32>(0.0, -1.0),
    );
    var o: VsOut;
    o.pos = vec4<f32>(pos[vi], 0.0, 1.0);
    o.uv = uv[vi];
    return o;
}

@group(0) @binding(0) var t: texture_2d<f32>;
@group(0) @binding(1) var s: sampler;

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(t, s, in.uv);
}
```

(The UV V-flip in `vs` is a likely point of trial-and-error. Sonnet: verify the
chrome strip ends up at the BOTTOM of the window and the overlay popup at the
TOP, matching today's softbuffer output. If inverted, flip the V.)

### Step 3 — wire into `apps/buffr/src/main.rs`

Replace these field declarations on `AppState` (around lines 1508-1509):

```rust
softbuffer_ctx: Option<softbuffer::Context<Arc<Window>>>,
softbuffer_surface: Option<softbuffer::Surface<Arc<Window>, Arc<Window>>>,
```

with:

```rust
renderer: Option<crate::render::Renderer>,
```

Update the `AppState::new` initializer (around lines 1684-1685) to init
`renderer: None`.

Add the `mod render;` line near the top of `main.rs`.

In `resumed()` (around lines 3742-3763), replace the `softbuffer::Context::new`
/ `softbuffer::Surface::new_unchecked` block with:

```rust
match crate::render::Renderer::new(window.clone()) {
    Ok(r) => self.renderer = Some(r),
    Err(err) => {
        warn!(error = %err, "wgpu renderer init failed");
        event_loop.exit();
        return;
    }
}
```

In `paint_chrome_with` (around lines 2115-2412):

- Drop the surface borrow / `surface.resize` / `buffer_mut` block.
- Drop the `damage: Vec<softbuffer::Rect>` accumulation and
  `present_with_damage` call at the end.
- Replace with:

```rust
let Some(renderer) = self.renderer.as_mut() else {
    return;
};
renderer.resize(width, height);
let res = renderer.frame(|buf, w, h| {
    // EXACT same code that previously wrote into the softbuffer
    // surface buf — statusline.paint, tab_strip.paint, OSR scale
    // loop, prompt/notice/overlay paints. NO changes inside the
    // closure.
});
if let Err(err) = res {
    warn!(error = %err, "wgpu frame failed");
}
```

Delete the `damage` Vec entirely. Delete the OSR damage-rect helper.

The pre-render geometry / Arc-queue snapshots stay — they ran before the surface
borrow today and run before `renderer.frame` tomorrow.

### Step 4 — `--x11` CLI flag (Linux only)

In the `clap` derive struct (search for `#[derive(Parser)]` near the top of
`main.rs`):

```rust
/// Force the X11 backend on Linux. No effect on macOS / Windows.
/// Useful for testing the X11 path on a Wayland session.
#[cfg(target_os = "linux")]
#[arg(long)]
x11: bool,
```

In `fn main()`, before constructing the `EventLoop`, branch on `cli.x11` (Linux
only):

```rust
let event_loop = {
    let mut builder = winit::event_loop::EventLoop::<BuffrUserEvent>::with_user_event();
    #[cfg(target_os = "linux")]
    {
        use winit::platform::x11::EventLoopBuilderExtX11;
        if cli.x11 {
            builder.with_x11();
        }
        // Default: winit auto-picks Wayland if WAYLAND_DISPLAY is
        // set, otherwise X11. No explicit call needed.
    }
    builder.build()?
};
```

Sonnet: confirm the exact API. winit 0.30 may name these `with_x11_force` or
similar — look up the current `platform::x11::EventLoopBuilderExtX11` trait in
the locked `Cargo.lock` winit version and match precisely. If `with_x11` is
chained (consumes & returns builder) instead of mutating, restructure
accordingly.

### Step 5 — CEF GPU acceleration switches

File: `crates/buffr-core/src/app.rs`, inside `on_before_command_line_processing`
(around lines 77-106).

Today the only switches added are `enable-features=...`, `no-sandbox`, and
(conditionally) `force-renderer-accessibility`.

Add the following (each with a comment explaining what it buys). Sonnet:
research current Chromium behavior — names are stable since Chromium 100ish but
verify against CEF 147's bundled Chromium.

```rust
// GPU compositing: turn on the page compositor on the GPU even in
// OSR mode. Without these, chrome://gpu reports "Software only" for
// canvas, WebGL, and video decode. CEF's OSR mode does NOT require
// software compositing — that's a historical default.
append_switch(command_line, "enable-gpu");
append_switch(command_line, "enable-gpu-compositing");
append_switch(command_line, "enable-gpu-rasterization");
append_switch(command_line, "enable-zero-copy");

// Chromium's GPU blocklist often disables hardware accel on Linux
// laptops with integrated GPUs. We accept the risk — modern Mesa
// drivers handle this fine.
append_switch(command_line, "ignore-gpu-blocklist");

// Append to enable-features instead of replacing — preserve existing
// UseOzonePlatform / VaapiVideoDecodeLinuxGL plus add
// AcceleratedVideoDecodeLinuxGL (encode/decode on the GPU on Linux)
// and CanvasOopRasterization. Restructure the existing
// append_switch_with_value("enable-features", ...) call to merge
// these into one comma-separated value.
//
// Final value: "UseOzonePlatform,VaapiVideoDecodeLinuxGL,\
//               AcceleratedVideoDecodeLinuxGL,VaapiVideoEncoder,\
//               CanvasOopRasterization"
```

After implementing: run buffr, navigate to `chrome://gpu`. Acceptance criterion:
"Canvas", "Compositing", "WebGL", "WebGL2", and "Video Decode" all show
"Hardware accelerated" instead of "Software only".

If any feature still reports software, do NOT keep adding switches blindly. Stop
and report which features remain software so we can decide whether the deeper
shared-texture path is needed.

### Step 6 — CI concurrency cancellation

File: `.github/workflows/ci.yml`. Add at top level (after `env:`, before
`jobs:`):

```yaml
concurrency:
  group: ${{ github.workflow }}-${{ github.ref }}
  cancel-in-progress: true
```

File: `.github/workflows/docs.yml`. Update existing concurrency group from
`group: docs` to:

```yaml
concurrency:
  group: ${{ github.workflow }}-${{ github.ref }}
  cancel-in-progress: true
```

Effect: pushing a new commit to a branch / PR cancels any in-flight CI run on
that same branch / PR. Different branches don't cancel each other (the
`${{ github.ref }}` keys them apart).

### Step 7 — verification

Run, in order. All must be clean:

```sh
rtk cargo fmt --all
rtk cargo clippy --workspace --all-targets -- -D warnings
rtk cargo build --workspace
rtk cargo test --workspace
```

Then manual:

1. `cargo run -p buffr` on Wayland — confirm window opens, page loads, chrome
   strip + statusline render at correct size, resize the window aggressively
   (drag corner around) → no chrome lag, no stretch.
2. `cargo run -p buffr -- --x11` on the same Wayland session → buffr launches
   under X11 backend. Resize → still smooth.
3. Navigate to `chrome://gpu` in BOTH the Wayland and `--x11` runs to confirm
   GPU works on both.

CI: verify all jobs in `.github/workflows/ci.yml` go green. The `linux` smoke
job runs buffr under Xvfb — wgpu must initialise against Xvfb's GL context. If
wgpu fails on Xvfb (no GPU), gate the init: try wgpu, on failure log + exit 0
from the smoke target. (The smoke target tolerates exit-on-init-fail for the
purposes of CI; real GPU validation happens on developer machines.)

Sonnet: if Xvfb breaks wgpu init in CI, install `mesa-vulkan-drivers` and
`libvulkan1` in the apt-get step + add `libgl1-mesa-dri` if not already there.
The smoke job should still pass.

## Files touched

Modified:

- `apps/buffr/Cargo.toml` — softbuffer → wgpu/pollster/bytemuck
- `apps/buffr/src/main.rs` — renderer integration, --x11 flag, AppState fields,
  resumed() init, paint_chrome_with body
- `crates/buffr-core/src/app.rs` — CEF GPU switches
- `.github/workflows/ci.yml` — concurrency block
- `.github/workflows/docs.yml` — concurrency group key per-ref
- `Cargo.lock` — auto

Created:

- `apps/buffr/src/render.rs` — wgpu Renderer

NOT modified:

- `crates/buffr-ui/*` — chrome paint code stays as-is
- `crates/buffr-modal/*` — modal engine untouched
- `crates/buffr-core/src/host.rs` / `osr.rs` — CEF host + OSR frame pipe
  untouched
- Any other crate — single-binary scope

## Acceptance

- [ ] `cargo fmt --all -- --check` clean
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` zero
- [ ] `cargo build --workspace` green
- [ ] `cargo test --workspace` green
- [ ] All `.github/workflows/ci.yml` jobs green on the PR
- [ ] Manual: Wayland resize stretch eliminated
- [ ] Manual: `--x11` flag forces X11 backend on Wayland
- [ ] Manual: `chrome://gpu` shows hardware accel for canvas / WebGL / video
      decode
- [ ] Concurrency cancellation working — second push to same branch cancels
      first run

## Out-of-band notes for sonnet

- All diagnostic `tracing::debug!` calls in the codebase STAY (per saved
  feedback). Don't strip resize / view_rect / on_paint logs.
- For transient visual artifacts during reconfigure, prefer the simplest fix
  (per saved feedback). The wgpu surface reconfigure ITSELF should eliminate the
  lag — don't add gap-fill heuristics on top.
- Push commits as you finish substeps so the user (working remote) can pull and
  test (per saved feedback).
- Conventional Commits format. No Claude attribution.
- Run `rtk` prefix on all shell commands per CLAUDE.md.
- If anything in this plan is wrong or contradicts the actual code, STOP and
  report — do NOT freelance.
