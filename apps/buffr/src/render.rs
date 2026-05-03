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

use std::sync::Arc;

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

    /// Upload new pixels if generation changed or dims differ. On a dim
    /// change the texture is reallocated. Returns true on dim change so
    /// the caller can refresh its uniform.
    #[allow(clippy::too_many_arguments)]
    fn maybe_upload(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        bgl: &wgpu::BindGroupLayout,
        sampler: &wgpu::Sampler,
        uniform_buf: &wgpu::Buffer,
        format: wgpu::TextureFormat,
        upload: &OsrUpload<'_>,
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
                wgpu::ImageCopyTexture {
                    texture: &self.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                upload.pixels,
                wgpu::ImageDataLayout {
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

pub struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    surface_format: wgpu::TextureFormat,

    /// Pipeline for the OSR quad — no blending (opaque).
    pipeline_osr: wgpu::RenderPipeline,
    /// Pipeline for the chrome quad — alpha blending.
    pipeline_chrome: wgpu::RenderPipeline,

    bind_group_layout: wgpu::BindGroupLayout,

    /// Linear-filter sampler — used for both OSR and chrome textures.
    /// OSR benefits from bilinear during transient resize stretch (softens
    /// the stale-frame upscale so the fresh-frame transition isn't a pop);
    /// chrome at logical size bilinear-stretches to physical — at integer
    /// scales glyph pixel boundaries align exactly and the result is crisp.
    sampler_linear: wgpu::Sampler,

    /// OSR texture + state.
    osr: Option<OsrTexture>,
    /// Uniform buffer for the OSR quad rect. Written before each draw.
    osr_uniform_buf: wgpu::Buffer,

    /// Chrome texture — logical-pixel sized (physical / scale, rounded).
    /// The chrome quad NDC rect is always fullscreen (-1..+1), so the GPU
    /// bilinear-stretches the logical texture to fill the physical surface.
    chrome_texture: wgpu::Texture,
    chrome_view: wgpu::TextureView,
    chrome_bind_group: wgpu::BindGroup,
    /// Uniform buffer for the chrome quad (always fullscreen).
    chrome_uniform_buf: wgpu::Buffer,
    /// CPU-side chrome buffer — sized at logical (chrome_lw × chrome_lh).
    chrome_cpu: Vec<u32>,

    /// Physical surface width/height (wgpu swap-chain size).
    width: u32,
    height: u32,
    /// Logical chrome width/height (physical / scale, rounded up to ≥1).
    /// The chrome texture and CPU buffer are allocated at this size.
    chrome_lw: u32,
    chrome_lh: u32,
}

impl Renderer {
    pub fn new(window: Arc<Window>) -> Result<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..Default::default()
        });

        let surface = instance
            .create_surface(window.clone())
            .context("create wgpu surface")?;

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .context("no suitable wgpu adapter")?;

        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("buffr-device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                memory_hints: wgpu::MemoryHints::default(),
            },
            None,
        ))
        .context("wgpu request_device failed")?;

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

        // Mailbox preferred — Fifo blocks `surface.get_current_texture()` on
        // vsync, and during fast resize that produced 90-150 ms acquire
        // stalls right after CEF dim changes (wgpu reconfigures swap chain
        // on each renderer.resize, then Fifo waits for the chain to settle
        // while RedrawRequested storms paint requests). Mailbox lets the
        // swap chain advance without stalling. The earlier subsurface
        // flicker that prompted the Fifo experiment turned out to be
        // SharedOsrFrame mutex contention (since fixed with mem::swap),
        // not Mailbox's swap-chain dynamics.
        let present_mode = if caps.present_modes.contains(&wgpu::PresentMode::Mailbox) {
            wgpu::PresentMode::Mailbox
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
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
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
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        let pipeline_osr = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("buffr-osr"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs",
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs",
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
            multiview: None,
            cache: None,
        });

        let pipeline_chrome = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("buffr-chrome"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs",
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs",
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
            multiview: None,
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

        let chrome_cpu = vec![0u32; (chrome_lw * chrome_lh) as usize];

        Ok(Self {
            surface,
            device,
            queue,
            config,
            surface_format,
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
            chrome_cpu,
            width,
            height,
            chrome_lw,
            chrome_lh,
        })
    }

    /// Reconfigure the surface + chrome texture for the new physical window size.
    /// Idempotent when dims are unchanged.
    pub fn resize(&mut self, w: u32, h: u32) {
        if w == self.width && h == self.height {
            return;
        }
        tracing::debug!(
            old_w = self.width,
            old_h = self.height,
            new_w = w,
            new_h = h,
            "renderer.resize"
        );
        self.width = w;
        self.height = h;
        self.config.width = w;
        self.config.height = h;
        self.surface.configure(&self.device, &self.config);
        // Keep chrome logical size proportional: scale it to the same ratio
        // as the physical resize (covers startup before set_logical_size is called).
        // The caller should call set_logical_size separately when the scale changes.
        self.reallocate_chrome();
    }

    /// Update the logical chrome dimensions (physical / scale, rounded).
    ///
    /// Called whenever the window scale factor changes so the chrome CPU buffer
    /// and texture are sized at logical pixels; the GPU bilinear-stretches the
    /// smaller texture to fill the physical surface. Idempotent when unchanged.
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
        self.reallocate_chrome();
    }

    fn reallocate_chrome(&mut self) {
        let (texture, view) = make_texture(
            &self.device,
            self.chrome_lw,
            self.chrome_lh,
            self.surface_format,
        );
        self.chrome_bind_group = make_bind_group(
            &self.device,
            &self.bind_group_layout,
            &self.chrome_uniform_buf,
            &view,
            &self.sampler_linear,
        );
        self.chrome_texture = texture;
        self.chrome_view = view;
        self.chrome_cpu
            .resize((self.chrome_lw * self.chrome_lh) as usize, 0u32);
    }

    /// Composite one frame.
    ///
    /// - `chrome_dirty`: when true, `paint_chrome` is called and the chrome
    ///   texture is re-uploaded. When false, the existing chrome texture is
    ///   reused without any CPU work.
    /// - `paint_chrome`: closure that paints the chrome strips into the
    ///   provided buffer (full window size, row-major BGRA u32). Only the
    ///   chrome rows should write opaque pixels (`0xFF_RR_GG_BB`); the CEF
    ///   region must be left at `0x00_00_00_00` so the OSR shows through.
    /// - `osr`: when `Some`, the OSR texture is conditionally uploaded (only
    ///   when `generation` changed or dimensions differ) and drawn as a quad
    ///   at `dst_rect`. When `None` (Windowed mode), only the chrome pass runs.
    pub fn frame<F>(
        &mut self,
        chrome_dirty: bool,
        paint_chrome: F,
        osr: Option<OsrUpload<'_>>,
    ) -> Result<()>
    where
        F: FnOnce(&mut [u32], usize, usize),
    {
        let w = self.chrome_lw as usize;
        let h = self.chrome_lh as usize;
        let t0 = std::time::Instant::now();

        // Chrome paint + upload at logical (DIP) size.
        // The chrome quad NDC is always fullscreen; the GPU bilinear-stretches
        // the logical texture to fill the physical surface.
        if chrome_dirty {
            // Zero the buffer first so previous chrome state doesn't bleed
            // into rows that are now transparent (e.g. after CEF rect shrinks).
            self.chrome_cpu.fill(0);
            paint_chrome(&mut self.chrome_cpu, w, h);
            let bytes: &[u8] = bytemuck::cast_slice(&self.chrome_cpu);
            self.queue.write_texture(
                wgpu::ImageCopyTexture {
                    texture: &self.chrome_texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                bytes,
                wgpu::ImageDataLayout {
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

        let t_chrome = t0.elapsed();

        // OSR upload + uniform update.
        let has_osr = if let Some(ref upload) = osr {
            if upload.width == 0 || upload.height == 0 {
                false
            } else {
                // Linear sampler for the OSR texture: when the stale CEF
                // frame is stretched to fill a larger browser_rect during
                // a resize, bilinear filtering produces a softened image
                // that's visually close to what the fresh frame will look
                // like — the moment CEF catches up the transition is a
                // gentle re-sharpening rather than a sudden pop from
                // blocky nearest-neighbour upscaling. At steady state
                // (OSR dims match browser_rect 1:1) bilinear is a no-op.
                let osr_entry = self.osr.get_or_insert_with(|| {
                    OsrTexture::new(
                        &self.device,
                        &self.bind_group_layout,
                        &self.sampler_linear,
                        &self.osr_uniform_buf,
                        self.surface_format,
                        upload.width,
                        upload.height,
                    )
                });
                osr_entry.maybe_upload(
                    &self.device,
                    &self.queue,
                    &self.bind_group_layout,
                    &self.sampler_linear,
                    &self.osr_uniform_buf,
                    self.surface_format,
                    upload,
                );
                // Update the OSR quad uniform to match dst_rect.
                // The quad always covers the full dst_rect and samples the
                // entire OSR texture (UV 0..1). When CEF's buffer lags the
                // window dims (during a live-resize the new buffer hasn't
                // arrived yet), the GPU sampler stretches the stale frame
                // to fill the whole CEF region — no letterbox at the edge.
                // Once CEF catches up (was_resized → new on_paint), the
                // upload dims match dst and the stretch becomes 1:1.
                let (dx, dy, dw, dh) = upload.dst_rect;
                let win_w = self.width as f32;
                let win_h = self.height as f32;
                // NDC: x left→right = -1→+1, y bottom→top = -1→+1.
                // Window pixels: row 0 = top, col 0 = left.
                let ndc_x0 = (dx as f32 / win_w) * 2.0 - 1.0;
                let ndc_x1 = ((dx as f32 + dw as f32) / win_w) * 2.0 - 1.0;
                // y=top in NDC is +1, pixel row dy=0 means top of window.
                let ndc_y1 = 1.0 - (dy as f32 / win_h) * 2.0;
                let ndc_y0 = 1.0 - ((dy as f32 + dh as f32) / win_h) * 2.0;
                let uni = QuadUniforms {
                    ndc: [ndc_x0, ndc_y1, ndc_x1, ndc_y0],
                    uv: [0.0, 0.0, 1.0, 1.0],
                };
                self.queue
                    .write_buffer(&self.osr_uniform_buf, 0, bytemuck::bytes_of(&uni));
                true
            }
        } else {
            false
        };

        let t_osr = t0.elapsed();

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
        let frame = {
            let mut tex = self.surface.get_current_texture();
            for retry in 0..2 {
                match tex {
                    Ok(ref f) => {
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
                        drop(tex);
                        self.surface.configure(&self.device, &self.config);
                        tex = self.surface.get_current_texture();
                    }
                    Err(wgpu::SurfaceError::Outdated | wgpu::SurfaceError::Lost) => {
                        tracing::debug!(retry, "wgpu surface: outdated/lost, reconfigure + retry");
                        self.surface.configure(&self.device, &self.config);
                        tex = self.surface.get_current_texture();
                    }
                    Err(wgpu::SurfaceError::Timeout) => {
                        tracing::warn!(
                            "wgpu surface: get_current_texture timed out, skipping frame"
                        );
                        return Ok(());
                    }
                    Err(wgpu::SurfaceError::OutOfMemory) => {
                        return Err(anyhow::anyhow!("wgpu surface OOM"));
                    }
                }
            }
            match tex {
                Ok(f) => {
                    let actual = (f.texture.width(), f.texture.height());
                    if actual != (self.width, self.height) {
                        tracing::warn!(
                            config_w = self.width,
                            config_h = self.height,
                            actual_w = actual.0,
                            actual_h = actual.1,
                            "wgpu surface: still mismatched after retry — skipping frame"
                        );
                        return Ok(());
                    }
                    f
                }
                Err(_) => return Ok(()),
            }
        };

        let t_acquire = t0.elapsed();
        let frame_view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("buffr-frame"),
            });

        {
            let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("buffr-rpass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &frame_view,
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
            });

            // OSR quad — opaque, underneath chrome.
            if has_osr && let Some(ref osr_tex) = self.osr {
                rpass.set_pipeline(&self.pipeline_osr);
                rpass.set_bind_group(0, &osr_tex.bind_group, &[]);
                rpass.draw(0..6, 0..1);
            }

            // Chrome quad — alpha blended on top.
            rpass.set_pipeline(&self.pipeline_chrome);
            rpass.set_bind_group(0, &self.chrome_bind_group, &[]);
            rpass.draw(0..6, 0..1);
        }

        self.queue.submit(std::iter::once(encoder.finish()));
        let t_submit = t0.elapsed();
        frame.present();
        let t_present = t0.elapsed();

        let chrome_us = t_chrome.as_micros() as u64;
        let osr_us = (t_osr - t_chrome).as_micros() as u64;
        let acquire_us = (t_acquire - t_osr).as_micros() as u64;
        let submit_us = (t_submit - t_acquire).as_micros() as u64;
        let present_us = (t_present - t_submit).as_micros() as u64;
        let total_us = t_present.as_micros() as u64;
        tracing::trace!(
            chrome_us,
            osr_us,
            acquire_us,
            submit_us,
            present_us,
            total_us,
            "renderer.frame",
        );
        if total_us > 16_000 {
            tracing::debug!(
                chrome_us,
                osr_us,
                acquire_us,
                submit_us,
                present_us,
                total_us,
                "renderer.frame: slow",
            );
        }

        Ok(())
    }
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
