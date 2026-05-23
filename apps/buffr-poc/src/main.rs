//! buffr-poc — minimal demo proving wayr + WPE WebKit subsurface
//! embedding works on Wayland.
//!
//! Architecture:
//!
//! - `wayr::EventLoop` opens the Wayland connection and binds the
//!   four globals WPE's `BuffrDisplayWayland` C subclass needs
//!   (`wl_display`, `wl_compositor`, `wl_subcompositor`, plus the
//!   parent `wl_surface` from a `wayr::Toplevel`).
//! - `buffr_webkit::WebKitBackend::open_engine` is called with
//!   `prefer_native = true` and the four handles. WPE constructs
//!   `BuffrDisplayWayland`, which creates its own `wl_subsurface`
//!   child of buffr's toplevel and renders into it.
//! - The result should be ONE Wayland top-level (buffr's window)
//!   with the WPE-rendered web content compositing inside it —
//!   instead of the two-windows symptom from the winit-based
//!   buffr-app where WPE created a separate `xdg_toplevel`.
//!
//! Verifies the core architectural claim: wayr's ownership of the
//! Wayland connection lets it expose the globals winit refused to
//! expose, which unblocks subsurface embedding without forks of
//! anything we don't control.

use anyhow::Context;
use buffr_engine::{
    Backend, BackendOpenOptions, BrowserEngine, EngineId, WaylandNativeHandles,
};
use std::sync::Arc;
use wayr::{
    ApplicationHandler, EventLoop, Size, Surface, SurfaceId, Toplevel, WindowEvent,
};

struct App {
    window: Option<Toplevel>,
    engine: Option<Arc<dyn BrowserEngine>>,
    backend: Arc<dyn Backend>,
    initial_url: String,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &mut EventLoop) {
        if self.window.is_some() {
            return;
        }

        // 1. Open a parent toplevel so we have a wl_surface to embed under.
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
            // BuffrDisplayWayland builds its own EGL display from
            // the wl_display ptr; we pass null.
            egl_display: std::ptr::null_mut(),
        };

        // 3. Open the WPE engine via the same buffr-webkit backend
        //    buffr-app uses, with the handles wayr just produced.
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
                tracing::info!("WPE engine opened; navigation should begin");
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
            }
            WindowEvent::CloseRequested => {
                tracing::info!("CloseRequested — exiting");
                event_loop.exit();
            }
            other => tracing::debug!(?surface_id, ?other, "event"),
        }
    }

    fn exiting(&mut self, _event_loop: &mut EventLoop) {
        // Drop engine before window so WPE's subsurface destroys
        // before the parent surface.
        self.engine.take();
        self.window.take();
    }
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| {
                    tracing_subscriber::EnvFilter::new("info,buffr_webkit=debug,wayr=debug")
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
            engine: None,
            backend,
            initial_url,
        })
        .map_err(anyhow::Error::from)
}
