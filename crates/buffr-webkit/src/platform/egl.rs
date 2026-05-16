//! EGL display + GLES context owned by the WPE worker thread.
//!
//! WPE WebKit 2.52 renders via accelerated compositing only — the SHM
//! exportable callback never fires. The EGL exportable path does fire, but
//! requires us to hand WPE FDO an EGL display via
//! `wpe_fdo_initialize_for_egl_display` and to keep a current GLES context
//! on the worker thread for any GL operations we want to perform on the
//! exported `EGLImageKHR`.
//!
//! The display + context live for the lifetime of the worker thread. No
//! window/pbuffer surface is created — `EGL_KHR_surfaceless_context` (Mesa
//! supports this everywhere) lets `eglMakeCurrent` use `EGL_NO_SURFACE`.

use khronos_egl as egl;
use std::sync::Arc;

use super::error::WebKitError;

/// Loaded EGL library + initialised display + GLES context.
pub(crate) struct EglWorker {
    pub(crate) egl: Arc<egl::DynamicInstance<egl::EGL1_5>>,
    pub(crate) display: egl::Display,
    pub(crate) context: egl::Context,
}

impl EglWorker {
    /// Open `EGL_DEFAULT_DISPLAY`, pick a GLES2 config, build a context.
    pub(crate) fn new() -> Result<Self, WebKitError> {
        // SAFETY: dlopens libEGL.so.1. Safe to call once per process; the
        // library is reference-counted by libloading.
        let egl = unsafe {
            egl::DynamicInstance::<egl::EGL1_5>::load_required().map_err(|e| {
                WebKitError::InitFailed(format!(
                    "failed to load libEGL.so.1 (khronos-egl dynamic): {e}"
                ))
            })?
        };
        let egl = Arc::new(egl);

        let display = unsafe { egl.get_display(egl::DEFAULT_DISPLAY) }
            .ok_or_else(|| WebKitError::InitFailed("eglGetDisplay(DEFAULT) returned NULL".into()))?;

        let (major, minor) = egl
            .initialize(display)
            .map_err(|e| WebKitError::InitFailed(format!("eglInitialize: {e}")))?;
        tracing::info!("webkit: EGL initialised {major}.{minor}");

        egl.bind_api(egl::OPENGL_ES_API)
            .map_err(|e| WebKitError::InitFailed(format!("eglBindAPI(GLES): {e}")))?;

        let config = {
            let attrs = [
                egl::SURFACE_TYPE,
                egl::PBUFFER_BIT,
                egl::RENDERABLE_TYPE,
                egl::OPENGL_ES2_BIT,
                egl::RED_SIZE,
                8,
                egl::GREEN_SIZE,
                8,
                egl::BLUE_SIZE,
                8,
                egl::ALPHA_SIZE,
                8,
                egl::NONE,
            ];
            egl.choose_first_config(display, &attrs)
                .map_err(|e| WebKitError::InitFailed(format!("eglChooseConfig: {e}")))?
                .ok_or_else(|| {
                    WebKitError::InitFailed("eglChooseConfig found no matching config".into())
                })?
        };

        let context = {
            let attrs = [egl::CONTEXT_CLIENT_VERSION, 2, egl::NONE];
            egl.create_context(display, config, None, &attrs)
                .map_err(|e| WebKitError::InitFailed(format!("eglCreateContext: {e}")))?
        };

        Ok(Self {
            egl,
            display,
            context,
        })
    }

    /// Bind the GLES context to the calling thread without an EGL surface
    /// (requires `EGL_KHR_surfaceless_context`).
    pub(crate) fn make_current(&self) -> Result<(), WebKitError> {
        self.egl
            .make_current(self.display, None, None, Some(self.context))
            .map_err(|e| WebKitError::InitFailed(format!("eglMakeCurrent: {e}")))
    }

    /// Raw `EGLDisplay` pointer for handing to `wpe_fdo_initialize_for_egl_display`.
    pub(crate) fn raw_display(&self) -> *mut std::ffi::c_void {
        self.display.as_ptr()
    }
}

impl Drop for EglWorker {
    fn drop(&mut self) {
        let _ = self.egl.make_current(self.display, None, None, None);
        let _ = self.egl.destroy_context(self.display, self.context);
        let _ = self.egl.terminate(self.display);
    }
}
