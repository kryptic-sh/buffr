//! buffr-poc — minimal demo proving wayr + WPE WebKit subsurface
//! embedding works on Wayland.
//!
//! Architecture:
//!
//! - `wayr::EventLoop` opens the Wayland connection and binds the
//!   four globals WPE's `BuffrDisplayWayland` C subclass needs
//!   (`wl_display`, `wl_compositor`, `wl_subcompositor`, plus the
//!   parent `wl_surface` from a `wayr::Toplevel`).
//! - The toplevel's chrome layer is painted by wgpu via wayr's
//!   `raw-window-handle 0.6` impl — clears to a colour every
//!   `RedrawRequested`. Without a chrome buffer the parent
//!   wl_surface stays unmapped and we can't actually see anything.
//! - `buffr_webkit::WebKitBackend::open_engine` is called with
//!   `prefer_native = true` and the four handles. WPE constructs
//!   `BuffrDisplayWayland`, which creates its own `wl_subsurface`
//!   child of the toplevel and renders into it.
//! - End state: ONE Wayland top-level with WPE's web content
//!   compositing inside it as a subsurface — instead of the
//!   two-window symptom from the winit-based buffr-app.

use anyhow::Context;
use buffr_engine::{
    Backend, BackendOpenOptions, BrowserEngine, EngineId, WaylandNativeHandles,
};
use std::sync::Arc;
use wayr::{
    ApplicationHandler, EventLoop, Size, Surface, SurfaceId, Toplevel, WindowEvent,
};

/// Render plumbing for the chrome (parent wl_surface). Holds the
/// wgpu pieces that need to live as long as the surface they were
/// configured for.
struct Chrome {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
}

impl Chrome {
    fn new(window: &Toplevel, event_loop: &EventLoop) -> anyhow::Result<Self> {
        use raw_window_handle::{
            DisplayHandle, HasDisplayHandle, HasWindowHandle, WindowHandle,
        };

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN | wgpu::Backends::GL,
            flags: wgpu::InstanceFlags::default(),
            backend_options: wgpu::BackendOptions::default(),
            memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
            display: None,
        });

        let display: DisplayHandle<'_> = event_loop.display_handle().context("display handle")?;
        let window_handle: WindowHandle<'_> = window.window_handle().context("window handle")?;

        // SAFETY: handles borrow `event_loop` + `window`; we
        // immediately extend the lifetime via the SurfaceTargetUnsafe
        // path because wgpu::Surface<'static> can't carry the
        // borrows. The actual surface drops before the window does
        // (we hold Chrome in App alongside the Toplevel; App's
        // exiting drops Chrome before Toplevel).
        let surface = unsafe {
            let target = wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: Some(display.as_raw()),
                raw_window_handle: window_handle.as_raw(),
            };
            instance.create_surface_unsafe(target)?
        };

        let adapter = pollster::block_on(instance.request_adapter(
            &wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::LowPower,
                compatible_surface: Some(&surface),
                ..Default::default()
            },
        ))
        .context("no wgpu adapter")?;
        tracing::info!(adapter = ?adapter.get_info().name, "wgpu adapter");

        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("buffr-poc chrome device"),
                ..Default::default()
            },
        ))?;

        let initial = window.size();
        let initial_w = initial.width.max(1);
        let initial_h = initial.height.max(1);

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: initial_w,
            height: initial_h,
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
        };
        surface.configure(&device, &config);
        Ok(Self {
            surface,
            device,
            queue,
            config,
        })
    }

    fn resize(&mut self, new_size: Size) {
        self.config.width = new_size.width.max(1);
        self.config.height = new_size.height.max(1);
        self.surface.configure(&self.device, &self.config);
    }

    fn paint(&self) {
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t)
            | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            other => {
                tracing::warn!(?other, "skipping frame");
                return;
            }
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("buffr-poc chrome encoder"),
            });
        {
            // Solid dark teal so the embedded WPE area is visually
            // distinct from the chrome.
            let _rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("chrome clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.05,
                            g: 0.10,
                            b: 0.13,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
        }
        self.queue.submit(Some(encoder.finish()));
        frame.present();
    }
}

struct App {
    window: Option<Toplevel>,
    chrome: Option<Chrome>,
    engine: Option<Arc<dyn BrowserEngine>>,
    backend: Arc<dyn Backend>,
    initial_url: String,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &mut EventLoop) {
        if self.window.is_some() {
            return;
        }

        // 1. Parent toplevel.
        let window = Toplevel::builder()
            .with_title("buffr-poc — wayr + WPE")
            .with_app_id("sh.kryptic.buffr.poc")
            .with_initial_size(Size::new(1280, 800))
            .build(event_loop)
            .expect("build parent toplevel");
        tracing::info!(parent_id = ?window.id(), "parent toplevel built");

        // 2. Pull every raw FFI pointer WPE needs.
        let wl_display = event_loop
            .wl_display_ptr()
            .expect("wl_display ptr available")
            .as_ptr();
        let wl_compositor = event_loop
            .wl_compositor_ptr()
            .expect("wl_compositor ptr available")
            .as_ptr();
        let wl_subcompositor = event_loop
            .wl_subcompositor_ptr()
            .expect("wl_subcompositor ptr available")
            .as_ptr();
        let parent_wl_surface = window
            .wl_surface_ptr()
            .expect("parent wl_surface ptr available")
            .as_ptr();
        tracing::info!(
            ?wl_display,
            ?wl_compositor,
            ?wl_subcompositor,
            ?parent_wl_surface,
            "extracted wayland handles from wayr"
        );

        let handles = WaylandNativeHandles {
            wl_display,
            parent_wl_surface,
            wl_compositor,
            wl_subcompositor,
            egl_display: std::ptr::null_mut(),
        };

        // 3. wgpu chrome — without this the parent wl_surface never
        //    attaches a buffer and the compositor doesn't map it.
        match Chrome::new(&window, event_loop) {
            Ok(chrome) => self.chrome = Some(chrome),
            Err(err) => {
                tracing::error!(error = %err, "wgpu chrome init failed");
                event_loop.exit();
                return;
            }
        }

        // 4. WPE engine through buffr-webkit (constructs
        //    BuffrDisplayWayland against our handles).
        let opts = BackendOpenOptions {
            engine_id: EngineId::new("webkit"),
            data_dir: None,
            cache_dir: None,
            initial_url: &self.initial_url,
            frame_rate: 60,
            device_scale: 1.0,
            initial_size: (1280, 800),
            private: true,
            history: None,
            download_dir: None,
            downloads: None,
            notice_queue: None,
            find_sink: None,
            sinks: Box::new(()),
            prefer_native: true,
            wayland_handles: Some(handles),
            internal_server: None,
        };
        match self.backend.open_engine(opts) {
            Ok(engine) => {
                tracing::info!("WPE engine opened");
                engine.osr_focus(true);
                self.engine = Some(engine);
            }
            Err(err) => {
                tracing::error!(error = %err, "open_engine failed");
                event_loop.exit();
                return;
            }
        }

        self.window = Some(window);

        // Trigger an initial paint so the parent surface attaches a
        // buffer and the compositor maps the window.
        if let Some(chrome) = self.chrome.as_ref() {
            chrome.paint();
        }
    }

    fn window_event(
        &mut self,
        event_loop: &mut EventLoop,
        surface_id: SurfaceId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::Resized(size) => {
                tracing::info!(?surface_id, w = size.width, h = size.height, "Resized");
                if let Some(chrome) = self.chrome.as_mut() {
                    chrome.resize(size);
                    chrome.paint();
                }
            }
            WindowEvent::RedrawRequested => {
                if let Some(chrome) = self.chrome.as_ref() {
                    chrome.paint();
                }
            }
            WindowEvent::CloseRequested => {
                tracing::info!("CloseRequested — exiting");
                event_loop.exit();
            }
            other => tracing::debug!(?surface_id, ?other, "event"),
        }
    }

    fn exiting(&mut self, _event_loop: &mut EventLoop) {
        // Drop order: engine → chrome → window.
        self.engine.take();
        self.chrome.take();
        self.window.take();
    }
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| {
                    tracing_subscriber::EnvFilter::new(
                        "info,buffr_webkit=debug,wayr=debug",
                    )
                }),
        )
        .init();

    let backend: Arc<dyn Backend> = Arc::new(buffr_webkit::WebKitBackend::new());
    backend
        .initialize("/tmp/buffr-poc-cache")
        .map_err(|e| anyhow::anyhow!(e))
        .context("backend init")?;

    let initial_url = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "https://example.com".to_string());

    let event_loop = EventLoop::<()>::new()?;
    event_loop
        .run_app(&mut App {
            window: None,
            chrome: None,
            engine: None,
            backend,
            initial_url,
        })
        .map_err(anyhow::Error::from)
}
