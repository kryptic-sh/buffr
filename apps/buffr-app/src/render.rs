//! wgpu-based present layer — two-texture GPU compositor.
//!
//! Architecture:
//!
//! 1. OSR texture — CEF's off-screen BGRA pixels at CEF's native resolution.
//!    Uploaded only when `OsrFrame::generation` changes. Drawn as a quad
//!    that covers `[cef_x, cef_y, cef_x+copy_w, cef_y+copy_h]` in window
//!    coords; the GPU `LoadOp::Clear` fills everything outside with the
//!    background colour.
//!
//! 2. Chrome texture — window-sized BGRA CPU buffer. Only the chrome strips
//!    (statusline, tab strip, popups) write opaque pixels (alpha = 0xFF).
//!    The CEF region rows stay at alpha = 0x00 so the OSR shows through.
//!    Re-uploaded only when `chrome_dirty` is true.
//!
//! Render pass order:
//!   LoadOp::Clear(bg) → OSR quad (opaque) → chrome quad (alpha blend).
//!
//! Shader uniforms: a small `QuadUniforms` buffer per pipeline holds the
//! quad's NDC rect and UV rect. Two uniform buffers, two bind groups per
//! pipeline, two draw calls per frame.
//!
//! Texture format: `Bgra8Unorm`. Chrome u32 layout: `0xFF_RR_GG_BB` for
//! opaque chrome pixels, `0x00_00_00_00` for transparent (CEF region).
//! OSR pixels arrive from CEF already as BGRA bytes — cast directly.
//!
//! Threading model:
//!
//! The UI thread performs ONLY `surface.get_current_texture()` (fast: 89-300 µs).
//! All other wgpu work — `queue.write_texture` for chrome and OSR,
//! encoder building, `queue.submit`, and `frame.present()` — runs on a
//! dedicated "wgpu-render" worker thread. This prevents Wayland compositor
//! backpressure from blocking the UI thread when `PresentMode::Fifo` is the
//! only available mode (observed 4.6 s blocks on `write_texture` when the
//! compositor holds the buffer for an occluded surface).

use std::iter::once;
use std::mem::ManuallyDrop;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context as _, Result};
use bytemuck::{Pod, Zeroable};
use winit::window::Window;

/// Per-quad uniform: NDC rect (`[x0, y0, x1, y1]`) and UV rect
/// (`[u0, v0, u1, v1]`). Passed via a uniform buffer so we don't need
/// the `PUSH_CONSTANTS` feature.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct QuadUniforms {
    /// NDC clip-space rect: x0, y0, x1, y1 (all in [-1, 1]).
    ndc: [f32; 4],
    /// UV rect: u0, v0, u1, v1 (all in [0, 1]).
    uv: [f32; 4],
}

/// WGSL shader. A single quad is rasterised from two triangles.
/// `QuadUniforms` drives both the vertex positions and UVs.
const SHADER: &str = r#"
struct QuadUniforms {
    ndc: vec4<f32>,
    uv:  vec4<f32>,
};
@group(0) @binding(0) var<uniform> quad: QuadUniforms;
@group(0) @binding(1) var t: texture_2d<f32>;
@group(0) @binding(2) var s: sampler;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) vi: u32) -> VsOut {
    // Two triangles forming a quad. Winding: CCW.
    // Vertices in NDC (x0,y0)-(x1,y1), UV (u0,v0)-(u1,v1).
    // Note: Wayland/wgpu NDC Y convention — y=-1 is bottom, y=+1 is top.
    // Row 0 of a CPU buffer is the window top → maps to v=0 not v=1.
    // So quad.ndc.y is the TOP (higher NDC value) and uv.y is 0.
    var xs = array<f32, 6>(
        quad.ndc.x, quad.ndc.x, quad.ndc.z,
        quad.ndc.x, quad.ndc.z, quad.ndc.z,
    );
    var ys = array<f32, 6>(
        quad.ndc.w, quad.ndc.y, quad.ndc.w,
        quad.ndc.y, quad.ndc.y, quad.ndc.w,
    );
    var us = array<f32, 6>(
        quad.uv.x, quad.uv.x, quad.uv.z,
        quad.uv.x, quad.uv.z, quad.uv.z,
    );
    var vs2 = array<f32, 6>(
        quad.uv.w, quad.uv.y, quad.uv.w,
        quad.uv.y, quad.uv.y, quad.uv.w,
    );
    var o: VsOut;
    o.pos = vec4<f32>(xs[vi], ys[vi], 0.0, 1.0);
    o.uv = vec2<f32>(us[vi], vs2[vi]);
    return o;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(t, s, in.uv);
}
"#;

/// Pending OSR frame to composite in the next `Renderer::frame` call.
pub struct OsrUpload<'a> {
    /// BGRA pixels straight from CEF.
    pub pixels: &'a [u8],
    pub width: u32,
    pub height: u32,
    pub generation: u64,
    /// Destination rect in window pixels: (x, y, w, h).
    pub dst_rect: (u32, u32, u32, u32),
}

/// Owned variant of `OsrUpload` — created inside `frame()` by cloning the
/// pixel slice. The clone cost (~220 µs at 30 GB/s for a 6.6 MB buffer)
/// is the price for moving the GPU work off the UI thread.
struct OsrUploadOwned {
    pixels: Vec<u8>,
    width: u32,
    height: u32,
    generation: u64,
    dst_rect: (u32, u32, u32, u32),
}

impl<'a> From<&OsrUpload<'a>> for OsrUploadOwned {
    fn from(u: &OsrUpload<'a>) -> Self {
        Self {
            pixels: u.pixels.to_vec(),
            width: u.width,
            height: u.height,
            generation: u.generation,
            dst_rect: u.dst_rect,
        }
    }
}

/// Command sent from the UI thread to the render worker per frame.
struct RenderCommand {
    /// Surface texture acquired on the UI thread.
    surface_texture: wgpu::SurfaceTexture,
    /// Physical surface dims at acquire time.
    width: u32,
    height: u32,
    /// Logical chrome dims at acquire time.
    chrome_lw: u32,
    chrome_lh: u32,
    /// Owned chrome pixels. `Some` only when `chrome_dirty` was true;
    /// `None` means the worker reuses its existing chrome texture.
    chrome_pixels: Option<Vec<u32>>,
    /// Owned OSR upload. `None` in windowed/no-OSR mode.
    osr: Option<OsrUploadOwned>,
}

/// Channel pair owned by `Renderer` on the UI side.
struct RenderChannel {
    /// Capacity-1 SyncSender → mailbox semantics: drop the incoming frame
    /// when the worker is still busy with the previous one (occluded surface).
    tx_cmd: std::sync::mpsc::SyncSender<RenderCommand>,
    rx_stats: std::sync::mpsc::Receiver<FrameStats>,
    handle: Option<std::thread::JoinHandle<()>>,
}

/// OSR GPU state: a single texture sized to whatever CEF most recently
/// emitted. The renderer GPU-stretches it (linear sampler) to fill the
/// live browser_rect, so when CEF's buffer dims lag the window dims the
/// stale frame visually scales to fit instead of letterboxing.
struct OsrTexture {
    #[allow(dead_code)]
    texture: wgpu::Texture,
    #[allow(dead_code)]
    view: wgpu::TextureView,
    bind_group: wgpu::BindGroup,
    width: u32,
    height: u32,
    last_generation: u64,
}

impl OsrTexture {
    fn new(
        device: &wgpu::Device,
        bgl: &wgpu::BindGroupLayout,
        sampler: &wgpu::Sampler,
        uniform_buf: &wgpu::Buffer,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
    ) -> Self {
        let (texture, view) = make_texture(device, width, height, format);
        let bind_group = make_bind_group(device, bgl, uniform_buf, &view, sampler);
        Self {
            texture,
            view,
            bind_group,
            width,
            height,
            last_generation: u64::MAX,
        }
    }

    /// Upload new pixels if generation changed or dims differ.
    /// On a dim change the texture is reallocated.
    /// Returns true on dim change so the caller can refresh its uniform.
    #[allow(clippy::too_many_arguments)]
    fn maybe_upload(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        bgl: &wgpu::BindGroupLayout,
        sampler: &wgpu::Sampler,
        uniform_buf: &wgpu::Buffer,
        format: wgpu::TextureFormat,
        upload: &OsrUploadOwned,
    ) -> bool {
        let dims_changed = upload.width != self.width || upload.height != self.height;
        if dims_changed {
            let (texture, view) = make_texture(device, upload.width, upload.height, format);
            self.bind_group = make_bind_group(device, bgl, uniform_buf, &view, sampler);
            self.texture = texture;
            self.view = view;
            self.width = upload.width;
            self.height = upload.height;
            self.last_generation = u64::MAX;
        }
        if upload.generation != self.last_generation {
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &self.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &upload.pixels,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(4 * upload.width),
                    rows_per_image: Some(upload.height),
                },
                wgpu::Extent3d {
                    width: upload.width,
                    height: upload.height,
                    depth_or_array_layers: 1,
                },
            );
            self.last_generation = upload.generation;
        }
        dims_changed
    }
}

/// All wgpu state that lives on the render worker thread.
struct RenderState {
    device: Arc<wgpu::Device>,
    queue: wgpu::Queue,
    pipeline_osr: wgpu::RenderPipeline,
    pipeline_chrome: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler_linear: wgpu::Sampler,

    /// OSR texture + state — `None` until the first OSR frame arrives.
    osr: Option<OsrTexture>,
    osr_uniform_buf: wgpu::Buffer,

    /// Chrome texture on the GPU — sized at logical (chrome_lw × chrome_lh).
    chrome_texture: wgpu::Texture,
    chrome_view: wgpu::TextureView,
    chrome_bind_group: wgpu::BindGroup,
    chrome_uniform_buf: wgpu::Buffer,

    /// Logical chrome dims as last seen by the worker.
    chrome_lw: u32,
    chrome_lh: u32,
    /// Physical surface dims as last seen by the worker.
    width: u32,
    height: u32,

    surface_format: wgpu::TextureFormat,
}

impl RenderState {
    /// Reallocate chrome texture + bind group for new logical dims.
    /// Called when the worker sees dims that differ from its cached state.
    fn reallocate_chrome(&mut self, chrome_lw: u32, chrome_lh: u32) {
        let (texture, view) = make_texture(&self.device, chrome_lw, chrome_lh, self.surface_format);
        self.chrome_bind_group = make_bind_group(
            &self.device,
            &self.bind_group_layout,
            &self.chrome_uniform_buf,
            &view,
            &self.sampler_linear,
        );
        self.chrome_texture = texture;
        self.chrome_view = view;
        self.chrome_lw = chrome_lw;
        self.chrome_lh = chrome_lh;
    }

    /// Write chrome pixels to the GPU texture.
    fn write_chrome(&self, pixels: &[u32]) {
        let bytes: &[u8] = bytemuck::cast_slice(pixels);
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.chrome_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bytes,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4 * self.chrome_lw),
                rows_per_image: Some(self.chrome_lh),
            },
            wgpu::Extent3d {
                width: self.chrome_lw,
                height: self.chrome_lh,
                depth_or_array_layers: 1,
            },
        );
    }

    /// Upload OSR pixels and update the OSR uniform (dst_rect → NDC).
    fn write_osr(&mut self, osr: &OsrUploadOwned) {
        let osr_entry = self.osr.get_or_insert_with(|| {
            OsrTexture::new(
                &self.device,
                &self.bind_group_layout,
                &self.sampler_linear,
                &self.osr_uniform_buf,
                self.surface_format,
                osr.width,
                osr.height,
            )
        });
        osr_entry.maybe_upload(
            &self.device,
            &self.queue,
            &self.bind_group_layout,
            &self.sampler_linear,
            &self.osr_uniform_buf,
            self.surface_format,
            osr,
        );
        // Update the OSR quad uniform to match dst_rect.
        let (dx, dy, dw, dh) = osr.dst_rect;
        let win_w = self.width as f32;
        let win_h = self.height as f32;
        let ndc_x0 = (dx as f32 / win_w) * 2.0 - 1.0;
        let ndc_x1 = ((dx as f32 + dw as f32) / win_w) * 2.0 - 1.0;
        let ndc_y1 = 1.0 - (dy as f32 / win_h) * 2.0;
        let ndc_y0 = 1.0 - ((dy as f32 + dh as f32) / win_h) * 2.0;
        let uni = QuadUniforms {
            ndc: [ndc_x0, ndc_y1, ndc_x1, ndc_y0],
            uv: [0.0, 0.0, 1.0, 1.0],
        };
        self.queue
            .write_buffer(&self.osr_uniform_buf, 0, bytemuck::bytes_of(&uni));
    }
}

/// Loop executed on the "wgpu-render" worker thread.
///
/// Blocks on `rx_cmd.recv()`. Each received `RenderCommand` contains the
/// already-acquired `SurfaceTexture` plus all pixel data needed for the
/// frame. The worker handles every blocking wgpu call:
/// `queue.write_texture`, `queue.submit`, and `surface_texture.present()`.
fn render_worker(
    mut state: RenderState,
    rx_cmd: std::sync::mpsc::Receiver<RenderCommand>,
    tx_stats: std::sync::mpsc::Sender<FrameStats>,
) {
    while let Ok(cmd) = rx_cmd.recv() {
        let render_start = Instant::now();

        // Reconcile worker's cached dims with what the UI thread sent.
        // The UI thread drives resize (surface.configure) and mirrors the
        // new dims into every RenderCommand. The worker updates its GPU
        // state lazily here rather than via a separate channel message.
        if cmd.chrome_lw != state.chrome_lw || cmd.chrome_lh != state.chrome_lh {
            tracing::debug!(
                old_lw = state.chrome_lw,
                old_lh = state.chrome_lh,
                new_lw = cmd.chrome_lw,
                new_lh = cmd.chrome_lh,
                "render_worker: chrome dims changed, reallocating"
            );
            state.reallocate_chrome(cmd.chrome_lw, cmd.chrome_lh);
        }
        // Mirror physical dims so OSR NDC calculations use the right surface size.
        state.width = cmd.width;
        state.height = cmd.height;

        // Write chrome texture if the UI thread sent new pixels.
        if let Some(ref pixels) = cmd.chrome_pixels {
            state.write_chrome(pixels);
        }

        // Write OSR texture if provided.
        let has_osr = if let Some(ref osr) = cmd.osr {
            if osr.width == 0 || osr.height == 0 {
                false
            } else {
                state.write_osr(osr);
                true
            }
        } else {
            false
        };

        // Build encoder + render passes.
        let frame_view = cmd
            .surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = state
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("buffr-frame"),
            });

        {
            let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("buffr-rpass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &frame_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0x1a as f64 / 255.0,
                            g: 0x1b as f64 / 255.0,
                            b: 0x26 as f64 / 255.0,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            // OSR quad — opaque, underneath chrome.
            if has_osr && let Some(ref osr_tex) = state.osr {
                rpass.set_pipeline(&state.pipeline_osr);
                rpass.set_bind_group(0, &osr_tex.bind_group, &[]);
                rpass.draw(0..6, 0..1);
            }

            // Chrome quad — alpha blended on top.
            rpass.set_pipeline(&state.pipeline_chrome);
            rpass.set_bind_group(0, &state.chrome_bind_group, &[]);
            rpass.draw(0..6, 0..1);
        }

        state.queue.submit(once(encoder.finish()));
        let submit_done_us = render_start.elapsed().as_micros() as u64;

        cmd.surface_texture.present();
        let present_us = (render_start.elapsed().as_micros() as u64).saturating_sub(submit_done_us);

        // Run wgpu's lazy-drop GC.  Without an explicit poll the
        // resources we dropped on the Rust side (old swapchain images
        // after surface.configure during a resize, old chrome
        // textures after reallocate_chrome, old OSR textures on dim
        // change) stay alive inside wgpu's internal queue until the
        // device is polled — sustained rapid resize / churn never
        // gets a chance to GC and the 32-bit GPU address space hits
        // OOM at `surface.get_current_texture` within ~5 seconds.
        //
        // PollType::Poll is non-blocking: it processes whatever
        // command buffers have completed since the last poll and
        // runs the matching destructors, without waiting for any
        // in-flight GPU work.  Called once per submitted frame so
        // every wgpu drop is reclaimed within at most one frame.
        let _ = state.device.poll(wgpu::PollType::Poll);

        tracing::trace!(submit_done_us, present_us, "render_worker: frame done");
        if submit_done_us > 16_000 || present_us > 16_000 {
            tracing::debug!(submit_done_us, present_us, "render_worker: slow frame");
        }

        // Ignore send errors — UI thread may be shutting down.
        let _ = tx_stats.send(FrameStats {
            present_us,
            submit_done_us,
        });
    }
}

/// UI-side renderer. Owns the `wgpu::Surface` and all sizing state.
/// All heavy GPU work is delegated to the render worker thread via
/// `RenderChannel`.
pub struct Renderer {
    // ManuallyDrop so the Drop impl can choose to leak these when the
    // render worker is stuck mid-present (Wayland backpressure). Dropping
    // wgpu::Surface while a SurfaceTexture is still owned by the worker
    // panics: "Surface cannot be destroyed because is still in use".
    surface: ManuallyDrop<wgpu::Surface<'static>>,
    config: wgpu::SurfaceConfiguration,
    // Arc-shared device: UI thread uses it for surface.configure() on resize;
    // the worker thread holds the same Arc for all GPU operations.
    // ManuallyDrop for the same shutdown-leak reason as `surface`.
    device: ManuallyDrop<Arc<wgpu::Device>>,

    /// Physical surface width/height (wgpu swap-chain size).
    width: u32,
    height: u32,
    /// Logical chrome width/height (physical / scale, rounded up to ≥1).
    chrome_lw: u32,
    chrome_lh: u32,

    render_chan: RenderChannel,
    /// Most-recent `FrameStats` received from the render worker.
    /// Lags one frame behind.
    last_present_stats: FrameStats,
    /// Count of `SurfaceTexture` acquisitions that have been sent to the
    /// worker but not yet `present()`-ed.
    ///
    /// `desired_maximum_frame_latency = 1` means wgpu allows AT MOST ONE
    /// outstanding acquired texture at a time. Calling
    /// `surface.get_current_texture()` while a previous frame is still
    /// in flight panics with "Surface image is already acquired".
    ///
    /// Incremented on successful `try_send` to the worker. Decremented when
    /// `FrameStats` arrives from the worker (one stats per presented frame).
    /// `frame()` skips acquire entirely when this is non-zero.
    frames_in_flight: u32,
    /// Resize requested while the worker still owned a SurfaceTexture.
    /// `surface.configure()` cannot be called with an outstanding
    /// SurfaceTexture (wgpu panics: "SurfaceOutput must be dropped
    /// before a new Surface is made"), so resize() defers the
    /// reconfigure into this slot. `frame()` applies it as soon as
    /// `frames_in_flight == 0`.
    pending_resize: Option<(u32, u32)>,
}

impl Renderer {
    pub fn new(window: Arc<Window>) -> Result<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });

        let surface = instance
            .create_surface(window.clone())
            .context("create wgpu surface")?;

        // Adapter selection ladder: prefer HighPerformance (discrete GPU /
        // hardware Vulkan), fall back to LowPower (integrated), finally
        // force a fallback adapter (llvmpipe / SwiftShader / software path)
        // so machines with broken Vulkan + broken DRI2 + no usable GL still
        // boot. Better to render slowly via CPU than refuse to start.
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .or_else(|_| {
            tracing::warn!(
                "wgpu: HighPerformance adapter unavailable; trying LowPower"
            );
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::LowPower,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            }))
        })
        .or_else(|_| {
            tracing::warn!(
                "wgpu: no hardware adapter; falling back to software (llvmpipe / SwiftShader)"
            );
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::LowPower,
                compatible_surface: Some(&surface),
                force_fallback_adapter: true,
            }))
        })
        .context("no suitable wgpu adapter (tried HighPerformance, LowPower, software)")?;
        tracing::info!(
            adapter_info = ?adapter.get_info(),
            "wgpu: adapter selected"
        );

        let (device_raw, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("buffr-device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                memory_hints: wgpu::MemoryHints::default(),
                experimental_features: Default::default(),
                trace: wgpu::Trace::Off,
            }))
            .context("wgpu request_device failed")?;
        // Wrap in Arc so UI thread (surface.configure) and worker thread
        // (texture/buffer operations) can both hold a reference without clone.
        let device = Arc::new(device_raw);

        let size = window.inner_size();
        let width = size.width.max(1);
        let height = size.height.max(1);

        let caps = surface.get_capabilities(&adapter);
        let surface_format = caps
            .formats
            .iter()
            .copied()
            .find(|f| *f == wgpu::TextureFormat::Bgra8Unorm)
            .unwrap_or_else(|| caps.formats[0]);

        // Prefer Opaque composite alpha.  WebKit's OSR pixels carry their
        // own alpha channel — Google's homepage background, for example,
        // ships with alpha < 255 in regions.  With a PreMultiplied
        // surface the compositor blends our window's per-pixel alpha
        // against the desktop background, producing visible semi-
        // transparency in the viewport.
        //
        // Opaque tells the compositor to discard our alpha channel and
        // treat the surface as fully opaque, which is the correct
        // semantic for OSR (we own all the pixels in the window).  The
        // native-compositing path (Phase 3 v1, BUFFR_WEBKIT_NATIVE=1)
        // needs PreMultiplied so the chrome quad's transparent region
        // lets the underlying wl_subsurface show through; until that
        // path is closed, OSR wins the default.
        let want_native = std::env::var_os("BUFFR_WEBKIT_NATIVE").is_some_and(|v| v == "1");
        let composite_alpha = pick_composite_alpha(&caps.alpha_modes, want_native);
        tracing::info!(
            ?composite_alpha,
            want_native,
            "wgpu composite_alpha selected"
        );

        // Preference order: Mailbox → Immediate → Fifo.
        //
        // Fifo blocks `surface.get_current_texture()` on vsync, and
        // during fast resize that produced 90-150 ms acquire stalls right
        // after CEF dim changes (wgpu reconfigures swap chain on each
        // renderer.resize, then Fifo waits for the chain to settle while
        // RedrawRequested storms paint requests).  WORSE: when the surface
        // is occluded (Wayland workspace switch, Hyprland) the compositor
        // refuses to release the buffer and `present()` blocks the UI
        // thread for multiple seconds — observed Ctrl+C unresponsiveness
        // because BuffrUserEvent::Shutdown couldn't be processed until
        // present returned.
        //
        // Mailbox lets the swap chain advance without stalling.  Immediate
        // is the same plus tearing — acceptable fallback when the GPU stack
        // doesn't expose Mailbox (e.g. Vulkan unavailable → GL backend).
        // Fifo last resort — write_texture/submit/present on backpressure now
        // block only the render worker, never the UI thread.
        let present_mode = if caps.present_modes.contains(&wgpu::PresentMode::Mailbox) {
            wgpu::PresentMode::Mailbox
        } else if caps.present_modes.contains(&wgpu::PresentMode::Immediate) {
            wgpu::PresentMode::Immediate
        } else {
            wgpu::PresentMode::Fifo
        };
        tracing::debug!(?present_mode, "wgpu surface present mode selected");

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            width,
            height,
            present_mode,
            alpha_mode: composite_alpha,
            view_formats: vec![],
            desired_maximum_frame_latency: 1,
        };
        surface.configure(&device, &config);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("buffr-quad"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });

        // Bind group layout: uniform + texture + sampler.
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("buffr-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        multisampled: false,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("buffr-pl"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        let pipeline_osr = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("buffr-osr"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let pipeline_chrome = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("buffr-chrome"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: Some(wgpu::BlendState {
                        color: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::SrcAlpha,
                            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                            operation: wgpu::BlendOperation::Add,
                        },
                        alpha: wgpu::BlendComponent::OVER,
                    }),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let sampler_linear = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("buffr-linear"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let osr_uniform_buf = make_uniform_buf(&device, "buffr-osr-uni");
        let chrome_uniform_buf = make_uniform_buf(&device, "buffr-chrome-uni");

        // Chrome starts at physical size (scale = 1.0 until the window reports
        // its monitor's DPI via set_logical_size). The GPU quad is always
        // fullscreen NDC, so if logical == physical the stretch is 1:1.
        let chrome_lw = width;
        let chrome_lh = height;

        let (chrome_texture, chrome_view) =
            make_texture(&device, chrome_lw, chrome_lh, surface_format);
        let chrome_bind_group = make_bind_group(
            &device,
            &bind_group_layout,
            &chrome_uniform_buf,
            &chrome_view,
            &sampler_linear,
        );

        // Write the chrome uniform once — it is always a fullscreen quad.
        write_fullscreen_uniform(&queue, &chrome_uniform_buf);

        // Capacity-1 sync channel → mailbox semantics (drop the incoming
        // frame if the worker is still busy with the previous one).
        let (tx_cmd, rx_cmd) = std::sync::mpsc::sync_channel::<RenderCommand>(1);
        // Unbounded response channel: UI thread drains lazily via try_recv.
        let (tx_stats, rx_stats) = std::sync::mpsc::channel::<FrameStats>();

        let worker_state = RenderState {
            device: device.clone(),
            queue,
            pipeline_osr,
            pipeline_chrome,
            bind_group_layout,
            sampler_linear,
            osr: None,
            osr_uniform_buf,
            chrome_texture,
            chrome_view,
            chrome_bind_group,
            chrome_uniform_buf,
            chrome_lw,
            chrome_lh,
            width,
            height,
            surface_format,
        };

        let handle = std::thread::Builder::new()
            .name("wgpu-render".to_owned())
            .spawn(move || render_worker(worker_state, rx_cmd, tx_stats))
            .context("spawn wgpu-render thread")?;

        let render_chan = RenderChannel {
            tx_cmd,
            rx_stats,
            handle: Some(handle),
        };

        Ok(Self {
            surface: ManuallyDrop::new(surface),
            config,
            device: ManuallyDrop::new(device),
            width,
            height,
            chrome_lw,
            chrome_lh,
            render_chan,
            last_present_stats: FrameStats::default(),
            frames_in_flight: 0,
            pending_resize: None,
        })
    }

    /// Reconfigure the surface + chrome dims for the new physical window size.
    /// Idempotent when dims are unchanged.
    ///
    /// If the render worker still owns a SurfaceTexture from a previous
    /// frame, the actual `surface.configure()` call is deferred — wgpu
    /// panics if you reconfigure with an outstanding SurfaceTexture. The
    /// next `frame()` call applies the deferred resize once the worker
    /// has presented and `frames_in_flight` drops to zero.
    pub fn resize(&mut self, w: u32, h: u32) {
        if w == self.width && h == self.height {
            return;
        }
        tracing::debug!(
            old_w = self.width,
            old_h = self.height,
            new_w = w,
            new_h = h,
            in_flight = self.frames_in_flight,
            "renderer.resize"
        );
        // Update tracked dims immediately so the next RenderCommand carries
        // them, but defer the surface.configure() call if the worker is busy.
        self.width = w;
        self.height = h;
        self.config.width = w;
        self.config.height = h;
        if self.frames_in_flight == 0 {
            self.surface.configure(&self.device, &self.config);
        } else {
            tracing::debug!(
                in_flight = self.frames_in_flight,
                "renderer.resize: deferring surface.configure (worker mid-present)"
            );
            self.pending_resize = Some((w, h));
        }
        self.chrome_lw = w;
        self.chrome_lh = h;
    }

    /// Update the logical chrome dimensions (physical / scale, rounded).
    ///
    /// Called whenever the window scale factor changes. The worker reconciles
    /// its chrome texture lazily when it processes the next RenderCommand
    /// that carries the new dims. Idempotent when unchanged.
    pub fn set_logical_size(&mut self, lw: u32, lh: u32) {
        let lw = lw.max(1);
        let lh = lh.max(1);
        if lw == self.chrome_lw && lh == self.chrome_lh {
            return;
        }
        tracing::debug!(
            old_lw = self.chrome_lw,
            old_lh = self.chrome_lh,
            new_lw = lw,
            new_lh = lh,
            "renderer.set_logical_size"
        );
        self.chrome_lw = lw;
        self.chrome_lh = lh;
    }

    /// Drain render-worker stats messages and update `last_present_stats`.
    /// Returns `Some(stats)` only when at least one new sample arrived
    /// since the previous call — callers use this to gate occlusion-heuristic
    /// updates so the same stats aren't observed twice.
    ///
    /// Must be called BEFORE any wgpu work this frame. After this refactor
    /// the UI thread does no GPU work itself (only `get_current_texture`),
    /// but draining early still gives the embedder the latest timing signal
    /// before the paint-policy guard decides whether to skip the frame.
    pub fn poll_present_stats(&mut self) -> Option<FrameStats> {
        let mut latest = None;
        while let Ok(s) = self.render_chan.rx_stats.try_recv() {
            self.last_present_stats = s;
            self.frames_in_flight = self.frames_in_flight.saturating_sub(1);
            latest = Some(s);
        }
        latest
    }

    /// Composite one frame.
    ///
    /// UI thread responsibilities:
    ///   1. Acquire `surface.get_current_texture()` (fast: ~89-300 µs).
    ///   2. Invoke `paint_chrome` closure when `chrome_dirty` (CPU-only).
    ///   3. Clone OSR pixel slice into an owned `Vec<u8>`.
    ///   4. Send `RenderCommand` to the worker via capacity-1 channel.
    ///
    /// The worker handles all blocking wgpu calls: `write_texture` (chrome
    /// and OSR), encoder building, `queue.submit`, and `present()`.
    ///
    /// On channel Full (worker still busy), the frame is dropped — mailbox
    /// semantics, UI thread never blocks.
    ///
    /// - `chrome_dirty`: when true, `paint_chrome` is called and chrome
    ///   pixels are sent to the worker for GPU upload. When false, the
    ///   worker reuses its existing chrome texture.
    /// - `paint_chrome`: closure that paints the chrome strips into the
    ///   provided buffer (logical chrome size, row-major BGRA u32). Only
    ///   the chrome rows should write opaque pixels (`0xFF_RR_GG_BB`); the
    ///   CEF region must be left at `0x00_00_00_00` so the OSR shows through.
    /// - `osr`: when `Some`, pixels are cloned and sent to the worker which
    ///   conditionally uploads to the OSR texture (only when generation
    ///   changed or dims differ). When `None`, only the chrome pass runs.
    pub fn frame<F>(
        &mut self,
        chrome_dirty: bool,
        paint_chrome: F,
        osr: Option<OsrUpload<'_>>,
    ) -> Result<FrameStats>
    where
        F: FnOnce(&mut [u32], usize, usize),
    {
        let t0 = Instant::now();

        // Drain stats from the previous frame before doing any work.
        // This also decrements `frames_in_flight` for each presented frame.
        self.poll_present_stats();

        // Apply any deferred resize now that the worker has drained.
        // resize() defers surface.configure when frames_in_flight > 0
        // (wgpu panics on reconfigure with an outstanding SurfaceTexture).
        if let Some((w, h)) = self.pending_resize
            && self.frames_in_flight == 0
        {
            tracing::debug!(w, h, "renderer.frame: applying deferred resize");
            self.surface.configure(&self.device, &self.config);
            self.pending_resize = None;
        }

        // Skip frame entirely if the worker hasn't presented the previous one.
        // wgpu's swapchain (with desired_maximum_frame_latency = 1) panics on
        // the second `get_current_texture()` if the first hasn't been
        // presented. Mailbox semantics: the embedder will request another
        // paint when state changes; dropping a frame here is harmless.
        if self.frames_in_flight > 0 {
            tracing::trace!(
                in_flight = self.frames_in_flight,
                "renderer.frame: worker still busy with previous frame, skipping"
            );
            return Ok(self.last_present_stats);
        }

        // Chrome CPU paint — only when dirty. The closure runs on the UI
        // thread because it captures AppState data that can't be sent to
        // the worker. The resulting Vec<u32> is sent to the worker which
        // uploads it to the GPU chrome texture.
        let chrome_pixels = if chrome_dirty {
            let lw = self.chrome_lw as usize;
            let lh = self.chrome_lh as usize;
            // Zero the buffer first so previous chrome state doesn't bleed
            // into rows that are now transparent (e.g. after CEF rect shrinks).
            let mut buf = vec![0u32; lw * lh];
            paint_chrome(&mut buf, lw, lh);
            Some(buf)
        } else {
            None
        };

        let t_chrome = t0.elapsed();

        // Clone OSR pixels into an owned buffer.
        let osr_owned = osr.as_ref().map(OsrUploadOwned::from);

        let t_osr_clone = t0.elapsed();

        // Acquire the swapchain texture.
        //
        // After a `surface.configure(new_size)` the swapchain still has
        // pre-allocated buffers at the previous size queued for present;
        // the next `get_current_texture()` may hand one back. If we
        // render into it and present, Hyprland (and other Wayland
        // compositors) letterboxes the mismatched buffer against the
        // newly-configured surface — visible as persistent black bars
        // that "lag one resize behind" because every subsequent acquire
        // also returns the previous-size buffer until the pipeline
        // drains. Reconfigure + retry once to flush the stale chain.
        //
        // wgpu 29: `get_current_texture()` returns `CurrentSurfaceTexture`
        // (an enum), not `Result`. Success variants carry a `SurfaceTexture`;
        // error variants signal what went wrong without a payload.
        let frame = {
            // Helper: extract SurfaceTexture from a success variant, or None.
            fn unwrap_surface_tex(
                cst: wgpu::CurrentSurfaceTexture,
            ) -> Option<wgpu::SurfaceTexture> {
                match cst {
                    wgpu::CurrentSurfaceTexture::Success(t)
                    | wgpu::CurrentSurfaceTexture::Suboptimal(t) => Some(t),
                    _ => None,
                }
            }

            tracing::debug!(target: "buffr::ui_path", "enter: surface.get_current_texture");
            let mut cst = self.surface.get_current_texture();
            tracing::debug!(target: "buffr::ui_path", "exit:  surface.get_current_texture");
            for retry in 0..2 {
                match &cst {
                    wgpu::CurrentSurfaceTexture::Success(f)
                    | wgpu::CurrentSurfaceTexture::Suboptimal(f) => {
                        let actual = (f.texture.width(), f.texture.height());
                        if actual == (self.width, self.height) {
                            break;
                        }
                        tracing::debug!(
                            config_w = self.width,
                            config_h = self.height,
                            actual_w = actual.0,
                            actual_h = actual.1,
                            retry,
                            "wgpu surface: stale-size swapchain texture, reconfigure + retry"
                        );
                        // Drop the stale texture before reconfigure so
                        // the swapchain can rebuild without a live
                        // reference outstanding.
                        drop(cst);
                        tracing::debug!(target: "buffr::ui_path", "enter: surface.configure (stale-size retry)");
                        self.surface.configure(&self.device, &self.config);
                        tracing::debug!(target: "buffr::ui_path", "exit:  surface.configure");
                        tracing::debug!(target: "buffr::ui_path", "enter: surface.get_current_texture (retry)");
                        cst = self.surface.get_current_texture();
                        tracing::debug!(target: "buffr::ui_path", "exit:  surface.get_current_texture (retry)");
                    }
                    wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                        tracing::debug!(retry, "wgpu surface: outdated/lost, reconfigure + retry");
                        tracing::debug!(target: "buffr::ui_path", "enter: surface.configure (outdated/lost retry)");
                        self.surface.configure(&self.device, &self.config);
                        tracing::debug!(target: "buffr::ui_path", "exit:  surface.configure");
                        tracing::debug!(target: "buffr::ui_path", "enter: surface.get_current_texture (outdated retry)");
                        cst = self.surface.get_current_texture();
                        tracing::debug!(target: "buffr::ui_path", "exit:  surface.get_current_texture (outdated retry)");
                    }
                    wgpu::CurrentSurfaceTexture::Timeout => {
                        tracing::warn!(
                            "wgpu surface: get_current_texture timed out, skipping frame"
                        );
                        return Ok(FrameStats::default());
                    }
                    wgpu::CurrentSurfaceTexture::Occluded => {
                        tracing::debug!("wgpu surface: occluded, skipping frame");
                        return Ok(FrameStats::default());
                    }
                    wgpu::CurrentSurfaceTexture::Validation => {
                        tracing::warn!("wgpu surface: validation error, skipping frame");
                        return Ok(FrameStats::default());
                    }
                }
            }
            match unwrap_surface_tex(cst) {
                Some(f) => {
                    let actual = (f.texture.width(), f.texture.height());
                    if actual != (self.width, self.height) {
                        tracing::warn!(
                            config_w = self.width,
                            config_h = self.height,
                            actual_w = actual.0,
                            actual_h = actual.1,
                            "wgpu surface: still mismatched after retry — skipping frame"
                        );
                        return Ok(FrameStats::default());
                    }
                    f
                }
                None => return Ok(FrameStats::default()),
            }
        };

        let t_acquire = t0.elapsed();

        let cmd = RenderCommand {
            surface_texture: frame,
            width: self.width,
            height: self.height,
            chrome_lw: self.chrome_lw,
            chrome_lh: self.chrome_lh,
            chrome_pixels,
            osr: osr_owned,
        };

        match self.render_chan.tx_cmd.try_send(cmd) {
            Ok(()) => {
                // Track outstanding frame so the next call to `frame()` won't
                // try to acquire another swapchain texture before this one is
                // presented.
                self.frames_in_flight += 1;
            }
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                // Should be unreachable now that we gate acquire on
                // frames_in_flight, but stays defensive.
                tracing::trace!("renderer.frame: render worker busy, dropping frame");
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                tracing::warn!("renderer.frame: render worker thread exited unexpectedly");
            }
        }

        let chrome_us = t_chrome.as_micros() as u64;
        let osr_clone_us = (t_osr_clone - t_chrome).as_micros() as u64;
        let acquire_us = (t_acquire - t_osr_clone).as_micros() as u64;
        // submit_done_us now reflects the WORKER-thread time (from the previous
        // frame's stats). This is the correct occlusion signal: when the
        // worker's submit blocks on compositor backpressure, submit_done_us
        // is large and the heuristic in main.rs trips correctly.
        let submit_done_us = self.last_present_stats.submit_done_us;
        let present_us_prev = self.last_present_stats.present_us;
        tracing::trace!(
            chrome_us,
            osr_clone_us,
            acquire_us,
            submit_done_us,
            present_us_prev,
            "renderer.frame",
        );
        if chrome_us > 16_000
            || acquire_us > 16_000
            || submit_done_us > 16_000
            || present_us_prev > 16_000
        {
            tracing::debug!(
                chrome_us,
                osr_clone_us,
                acquire_us,
                submit_done_us,
                present_us_prev,
                "renderer.frame: slow",
            );
        }

        Ok(FrameStats {
            present_us: self.last_present_stats.present_us,
            submit_done_us,
        })
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        // Close the cmd channel so the worker's recv() loop exits as
        // soon as it returns from whatever GPU call it's currently in.
        let (dummy_tx, _dummy_rx) = std::sync::mpsc::sync_channel(0);
        let real_tx = std::mem::replace(&mut self.render_chan.tx_cmd, dummy_tx);
        drop(real_tx);

        // Drain any final stats the worker sent — this also decrements
        // frames_in_flight, so the "no outstanding present" branch can
        // pick up frames that completed between our last poll and now.
        while self.render_chan.rx_stats.try_recv().is_ok() {
            self.frames_in_flight = self.frames_in_flight.saturating_sub(1);
        }

        if self.frames_in_flight == 0 {
            // Worker is idle. Join cleanly, then drop wgpu state normally.
            if let Some(h) = self.render_chan.handle.take() {
                let _ = h.join();
            }
            // SAFETY: nothing else aliases `surface` / `device`; called once
            // in Drop; ManuallyDrop ensures no double-free.
            unsafe {
                ManuallyDrop::drop(&mut self.surface);
                ManuallyDrop::drop(&mut self.device);
            }
            return;
        }

        // Worker is mid-present. Joining would block multi-seconds on
        // Wayland compositor backpressure (observed 4.7 s on Hyprland
        // workspace switch). Detach the thread AND leak surface + device:
        // the worker still owns a SurfaceTexture borrowing from Surface,
        // so dropping Surface here would panic with "Surface cannot be
        // destroyed because is still in use". Leaking is fine — process
        // is exiting; OS reaps the thread and reclaims GPU resources.
        tracing::debug!(
            in_flight = self.frames_in_flight,
            "renderer drop: worker mid-present, detaching + leaking wgpu state"
        );
        if let Some(h) = self.render_chan.handle.take() {
            drop(h);
        }
        // surface and device intentionally NOT dropped (ManuallyDrop never
        // gets its inner Drop invoked).
    }
}

/// Per-frame timing stats returned by [`Renderer::frame`].
///
/// Used by the embedder's occlusion heuristic: a sustained jump in
/// `present_us` or `submit_done_us` is the most reliable signal that the
/// compositor stopped showing our surface (Wayland workspace switch,
/// minimize, fully covered window) on platforms where winit doesn't
/// fire `WindowEvent::Occluded` reliably.
#[derive(Debug, Clone, Copy, Default)]
pub struct FrameStats {
    /// Microseconds spent inside `wgpu::SurfaceTexture::present()` on the
    /// render worker thread. Healthy: <16 ms. Compositor-throttled
    /// invisible surface: 100 ms – 1.5 s. Carries the PREVIOUS frame's
    /// present time (one-frame lag because the worker sends stats after
    /// present() returns, and we drain before sending the next texture).
    pub present_us: u64,
    /// Microseconds spent on the render worker thread from receiving the
    /// `RenderCommand` until `queue.submit()` returned — includes all
    /// `write_texture` calls. Healthy: <16 ms. When the compositor
    /// backpressures the GPU queue, `queue.write_texture` and `queue.submit`
    /// block on the WORKER thread (not the UI thread) and `submit_done_us`
    /// balloons to seconds — a SAME-frame signal that complements the
    /// lagged `present_us`.
    pub submit_done_us: u64,
}

fn make_texture(
    device: &wgpu::Device,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
) -> (wgpu::Texture, wgpu::TextureView) {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("buffr-tex"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    (texture, view)
}

fn make_uniform_buf(device: &wgpu::Device, label: &str) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: std::mem::size_of::<QuadUniforms>() as u64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

fn make_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    uniform_buf: &wgpu::Buffer,
    view: &wgpu::TextureView,
    sampler: &wgpu::Sampler,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("buffr-bg"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buf.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(view),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
        ],
    })
}

/// Write a fullscreen quad uniform (NDC [-1,1]×[-1,1], UV [0,1]×[0,1]).
fn write_fullscreen_uniform(queue: &wgpu::Queue, buf: &wgpu::Buffer) {
    let uni = QuadUniforms {
        ndc: [-1.0, 1.0, 1.0, -1.0],
        uv: [0.0, 0.0, 1.0, 1.0],
    };
    queue.write_buffer(buf, 0, bytemuck::bytes_of(&uni));
}

/// Select the wgpu surface's composite-alpha mode from the compositor's
/// advertised capabilities and the live "native compositing wanted"
/// flag.  Pure function so the priority logic has a unit test.
///
/// Default path (OSR, `want_native = false`):
///   Opaque → Auto → PreMultiplied → PostMultiplied.
/// WebKit's OSR pixel buffer carries a real alpha channel — Google's
/// homepage background, for example, ships with alpha < 255 in regions
/// of the rendered output.  With a PreMultiplied surface the
/// compositor blends our window against the desktop, producing the
/// visible bleed-through reported during webkit verification.  Opaque
/// tells the compositor to discard our alpha channel; correct for OSR
/// because we own every pixel in the window.
///
/// Native path (`BUFFR_WEBKIT_NATIVE=1`, `want_native = true`):
///   PreMultiplied → PostMultiplied → Auto → Opaque.
/// The chrome quad's browser region is fully transparent (alpha = 0)
/// so the underlying wl_subsurface shows through.  Needs per-pixel
/// alpha on the surface.
///
/// Falls back to Opaque when the compositor doesn't advertise any of
/// the listed modes — defensive default; wgpu's surface configure
/// would error later if Opaque is also unsupported, but in practice
/// every Wayland / X11 / macOS / Windows compositor supports it.
fn pick_composite_alpha(
    advertised: &[wgpu::CompositeAlphaMode],
    want_native: bool,
) -> wgpu::CompositeAlphaMode {
    let priority: &[wgpu::CompositeAlphaMode] = if want_native {
        &[
            wgpu::CompositeAlphaMode::PreMultiplied,
            wgpu::CompositeAlphaMode::PostMultiplied,
            wgpu::CompositeAlphaMode::Auto,
            wgpu::CompositeAlphaMode::Opaque,
        ]
    } else {
        &[
            wgpu::CompositeAlphaMode::Opaque,
            wgpu::CompositeAlphaMode::Auto,
            wgpu::CompositeAlphaMode::PreMultiplied,
            wgpu::CompositeAlphaMode::PostMultiplied,
        ]
    };
    priority
        .iter()
        .find(|m| advertised.contains(m))
        .copied()
        .unwrap_or(wgpu::CompositeAlphaMode::Opaque)
}

#[cfg(test)]
mod composite_alpha_tests {
    use super::*;

    #[test]
    fn osr_path_prefers_opaque_when_available() {
        // Regression: viewport rendered semi-transparent against the
        // desktop on the default OSR path because PreMultiplied was
        // picked first.  Opaque MUST win on the non-native path.
        let advertised = vec![
            wgpu::CompositeAlphaMode::PreMultiplied,
            wgpu::CompositeAlphaMode::PostMultiplied,
            wgpu::CompositeAlphaMode::Opaque,
            wgpu::CompositeAlphaMode::Auto,
        ];
        assert_eq!(
            pick_composite_alpha(&advertised, /* want_native */ false),
            wgpu::CompositeAlphaMode::Opaque
        );
    }

    #[test]
    fn native_path_prefers_premultiplied_for_subsurface_transparency() {
        // Phase 3 native compositing needs per-pixel alpha so the
        // chrome quad's transparent browser region lets the
        // wl_subsurface show through.  PreMultiplied first.
        let advertised = vec![
            wgpu::CompositeAlphaMode::Opaque,
            wgpu::CompositeAlphaMode::PreMultiplied,
            wgpu::CompositeAlphaMode::PostMultiplied,
            wgpu::CompositeAlphaMode::Auto,
        ];
        assert_eq!(
            pick_composite_alpha(&advertised, /* want_native */ true),
            wgpu::CompositeAlphaMode::PreMultiplied
        );
    }

    #[test]
    fn osr_path_falls_through_priority_when_opaque_missing() {
        // Compositor advertises only the alpha-blended modes.  The
        // OSR path's priority then has to settle for Auto — the
        // next-best "compositor decides" semantics.
        let advertised = vec![
            wgpu::CompositeAlphaMode::PreMultiplied,
            wgpu::CompositeAlphaMode::Auto,
        ];
        assert_eq!(
            pick_composite_alpha(&advertised, false),
            wgpu::CompositeAlphaMode::Auto
        );
    }

    #[test]
    fn native_path_falls_through_to_opaque_when_blended_missing() {
        // Pathological compositor advertising only Opaque.  Native
        // path still resolves — the chrome quad simply won't have
        // its transparent region honoured, but the engine won't crash
        // and there's nothing more to do at this layer.
        let advertised = vec![wgpu::CompositeAlphaMode::Opaque];
        assert_eq!(
            pick_composite_alpha(&advertised, true),
            wgpu::CompositeAlphaMode::Opaque
        );
    }

    #[test]
    fn empty_advertised_falls_back_to_opaque() {
        // Defensive default.  Empty caps shouldn't happen in practice
        // (wgpu wouldn't have returned a SurfaceCapabilities), but
        // the priority logic must not panic.
        assert_eq!(
            pick_composite_alpha(&[], false),
            wgpu::CompositeAlphaMode::Opaque
        );
        assert_eq!(
            pick_composite_alpha(&[], true),
            wgpu::CompositeAlphaMode::Opaque
        );
    }
}
