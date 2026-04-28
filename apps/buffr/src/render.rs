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
//!
//! Texture format: `Bgra8Unorm`. The CPU buffer uses u32 with
//! `(r << 16) | (g << 8) | b` packing. On little-endian, that u32
//! lays out as `[B, G, R, 0x00]` in memory — BGRA. Linear, not sRGB.
//! Present mode: `Fifo` (vsync).

use std::sync::Arc;

use anyhow::{Context as _, Result};
use winit::window::Window;

const SHADER: &str = r#"
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
    // UV convention: (0,0) = top-left of texture.
    // Row 0 of the CPU buffer is the top of the window, so Y in NDC
    // (-1 = bottom) needs to map to V=1 and Y=+1 (top) to V=0.
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
"#;

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
    pub fn new(window: Arc<Window>) -> Result<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..Default::default()
        });

        // Safety: `window` is kept alive inside this struct (and by the
        // caller's `AppState`) for as long as the Surface exists.
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

        // Prefer Bgra8Unorm to match the CPU buffer's byte layout.
        let caps = surface.get_capabilities(&adapter);
        let surface_format = caps
            .formats
            .iter()
            .copied()
            .find(|f| *f == wgpu::TextureFormat::Bgra8Unorm)
            .unwrap_or_else(|| caps.formats[0]);

        // Prefer Mailbox over Fifo. Both are vsync-aligned, but Mailbox
        // is non-blocking: get_current_texture never waits on compositor
        // image release, since a newer frame replaces the queued one.
        // Under Fifo (any frame_latency) we observed get_current_texture
        // blocking 100ms-1s during typing bursts on Wayland because the
        // compositor stalls briefly on input events. Mailbox sidesteps
        // that entirely — we may drop intermediate frames if we render
        // faster than vsync, which is fine for UI: the latest one wins.
        // Falls back to Fifo when Mailbox is unsupported.
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
            label: Some("buffr-fullscreen"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("buffr-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        multisampled: false,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("buffr-pipeline-layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("buffr-pipeline"),
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

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("buffr-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let (texture, view) = make_texture(&device, width, height, surface_format);

        let bind_group = make_bind_group(&device, &bind_group_layout, &view, &sampler);

        let cpu_buf = vec![0u32; (width * height) as usize];

        Ok(Self {
            surface,
            device,
            queue,
            config,
            pipeline,
            texture,
            view,
            sampler,
            bind_group_layout,
            bind_group,
            cpu_buf,
            width,
            height,
        })
    }

    /// Reconfigure the surface + reallocate texture to the new size.
    /// Idempotent if dims unchanged.
    pub fn resize(&mut self, w: u32, h: u32) {
        if w == self.width && h == self.height {
            return;
        }
        self.width = w;
        self.height = h;
        self.config.width = w;
        self.config.height = h;
        self.surface.configure(&self.device, &self.config);
        let (texture, view) = make_texture(&self.device, w, h, self.config.format);
        self.bind_group =
            make_bind_group(&self.device, &self.bind_group_layout, &view, &self.sampler);
        self.texture = texture;
        self.view = view;
        self.cpu_buf.resize((w * h) as usize, 0u32);
    }

    /// Paint into the CPU buffer via `paint`, upload to the texture,
    /// draw, present. `paint` receives `(buf, width, height)` exactly
    /// as the previous softbuffer flow handed it.
    pub fn frame<F>(&mut self, paint: F) -> Result<()>
    where
        F: FnOnce(&mut [u32], usize, usize),
    {
        let w = self.width as usize;
        let h = self.height as usize;

        let t0 = std::time::Instant::now();

        // Invoke the paint closure with the CPU buffer.
        paint(&mut self.cpu_buf, w, h);

        let t_paint = t0.elapsed();

        // Upload CPU buffer to GPU texture as raw bytes.
        let bytes: &[u8] = bytemuck::cast_slice(&self.cpu_buf);
        self.queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bytes,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(4 * self.width),
                rows_per_image: Some(self.height),
            },
            wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
        );

        let t_upload = t0.elapsed();

        // get_current_texture can return SurfaceError::{Timeout,
        // Outdated, Lost, OutOfMemory}. On Outdated / Lost we
        // reconfigure the surface so the next frame can recover; on
        // Timeout (1s default) we just skip this frame to avoid
        // burning more time. Without recovery the surface stays
        // wedged and every subsequent frame stalls another ~1s.
        let frame = match self.surface.get_current_texture() {
            Ok(f) => f,
            Err(wgpu::SurfaceError::Timeout) => {
                tracing::warn!("wgpu surface: get_current_texture timed out, skipping frame");
                return Ok(());
            }
            Err(wgpu::SurfaceError::Outdated | wgpu::SurfaceError::Lost) => {
                // Common right after a Wayland configure: swapchain was
                // recreated but the wl_surface hasn't been committed at
                // the new size yet, so the Vulkan/EGL acquire reports
                // Outdated. Reconfigure and retry once before giving up.
                // Skipping the frame outright leaves the compositor with
                // the previous-size buffer, which it then stretches /
                // letterboxes onto the new surface — visible as the
                // bottom chrome trailing the new window edge during a
                // live drag.
                tracing::warn!("wgpu surface: outdated/lost, reconfigure + retry");
                self.surface.configure(&self.device, &self.config);
                match self.surface.get_current_texture() {
                    Ok(f) => f,
                    Err(_) => return Ok(()),
                }
            }
            Err(e @ wgpu::SurfaceError::OutOfMemory) => {
                return Err(anyhow::anyhow!("wgpu surface OOM: {e:?}"));
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
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });

            rpass.set_pipeline(&self.pipeline);
            rpass.set_bind_group(0, &self.bind_group, &[]);
            rpass.draw(0..3, 0..1);
        }

        self.queue.submit(std::iter::once(encoder.finish()));
        let t_submit = t0.elapsed();
        frame.present();
        let t_present = t0.elapsed();

        let paint_us = t_paint.as_micros() as u64;
        let upload_us = (t_upload - t_paint).as_micros() as u64;
        let acquire_us = (t_acquire - t_upload).as_micros() as u64;
        let submit_us = (t_submit - t_acquire).as_micros() as u64;
        let present_us = (t_present - t_submit).as_micros() as u64;
        let total_us = t_present.as_micros() as u64;
        tracing::trace!(
            paint_us,
            upload_us,
            acquire_us,
            submit_us,
            present_us,
            total_us,
            "renderer.frame",
        );
        // Surface a slow-frame breakdown at debug level when total
        // exceeds vsync budget. This stays visible without needing
        // RUST_LOG=trace so we can pinpoint which sub-step stalls.
        if total_us > 16_000 {
            tracing::debug!(
                paint_us,
                upload_us,
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

fn make_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    view: &wgpu::TextureView,
    sampler: &wgpu::Sampler,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("buffr-bg"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
        ],
    })
}
