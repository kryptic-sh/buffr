//! GLib worker thread for the WPE WebKit backend.
//!
//! # Threading model
//!
//! ```text
//! WebKitEngine (any thread — Send + Sync)
//!   cmd_tx ──── Command ────▶  wpe_thread (mpsc::SyncSender)
//!                                ├─ glib::MainLoop::run()
//!                                ├─ timeout_add_local (10 ms poll) ──▶ drain commands
//!                                └─ WpeRuntime (WebView + FDO exportable)
//!   engine_state ──────────────────────────────▶ Arc<Mutex<EngineState>>
//!                (written by GLib signal handlers on the worker thread)
//! ```
//!
//! WPE WebKit has no process-wide default MainContext ownership concept
//! (unlike GTK4), so the worker creates its own `glib::MainLoop::new(None, false)`
//! and immediately owns it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use buffr_engine::{SharedOsrFrame, SharedOsrViewState, TabId, TabSummary};

use super::error::WebKitError;
use super::runtime::{TabInfo, WpeRuntime};

// ── EngineState ───────────────────────────────────────────────────────────────

/// Thread-safe engine snapshot. Updated by the worker thread; read from any
/// thread behind `Mutex<EngineState>`.
pub(crate) struct EngineState {
    /// Open tabs in strip order.
    pub tabs: Vec<TabInfo>,
    /// Index of the active tab.
    pub active_idx: Option<usize>,
    /// Next tab id counter.
    pub next_id: u64,
    /// Current viewport width.
    pub width: u32,
    /// Current viewport height.
    pub height: u32,
    /// Set by signal handlers to signal a URL/title change.
    pub address_changed: bool,
    /// OSR sleep flag: true = skip repaints.
    pub osr_sleeping: Arc<AtomicBool>,
    /// Cached audio-active state (polled on the worker thread).
    pub audio_active: Arc<AtomicBool>,
}

impl EngineState {
    pub(crate) fn new(width: u32, height: u32) -> Self {
        Self {
            tabs: Vec::new(),
            active_idx: None,
            next_id: 1,
            width,
            height,
            address_changed: false,
            osr_sleeping: Arc::new(AtomicBool::new(false)),
            audio_active: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn active_tab_info(&self) -> Option<&TabInfo> {
        self.active_idx.and_then(|i| self.tabs.get(i))
    }

    pub(crate) fn tabs_summary(&self) -> Vec<TabSummary> {
        self.tabs.iter().map(|t| t.to_summary()).collect()
    }
}

// ── Commands ──────────────────────────────────────────────────────────────────

pub(crate) enum Command {
    OpenTab {
        url: String,
        reply: mpsc::SyncSender<Result<TabId, String>>,
        /// When true, the new tab is created in the background: no active-tab
        /// switch, no is_loading_atomic update, no needs_fresh reset.
        background: bool,
    },
    Navigate {
        url: String,
    },
    Resize {
        width: u32,
        height: u32,
    },
    OsrResize {
        width: u32,
        height: u32,
    },
    KeyEvent {
        ev: WpeKeyEvent,
    },
    MouseMove {
        x: i32,
        y: i32,
        modifiers: u32,
    },
    MouseClick {
        x: i32,
        y: i32,
        button: u32,
        pressed: bool,
        modifiers: u32,
    },
    MouseWheel {
        x: i32,
        y: i32,
        delta_x: i32,
        delta_y: i32,
        modifiers: u32,
    },
    Focus {
        focused: bool,
    },
    OsrSleep {
        sleep: bool,
    },
    /// Close the active tab. Drops the WebView and clears engine_state.tabs.
    CloseActive {
        reply: mpsc::SyncSender<bool>,
    },
    /// Close the tab with the given id. If it's not the active tab, just
    /// removes it and adjusts active_idx. If it is active, falls back like
    /// close_active (picks the next tab in strip order).
    CloseTab {
        id: TabId,
        reply: mpsc::SyncSender<bool>,
    },
    /// Switch the active tab to the one with this id. Flips the per-tab
    /// is_active flags so only the new active tab's paints / load-state
    /// updates reach the shared frame and the runtime is_loading atomic.
    SelectTab {
        id: TabId,
    },
    /// Run a JavaScript snippet in the active tab's main world. Used
    /// by `WebKitEngine::dispatch` to implement vim-style scrolling
    /// (`window.scrollBy(...)`) and other catch-all PageActions.
    EvalJs {
        script: String,
    },
    Shutdown,
}

/// Neutral keyboard event carried over the command channel.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WpeKeyEvent {
    pub key_code: u32,
    pub hardware_key_code: u32,
    pub pressed: bool,
    pub modifiers: u32,
}

// ── WorkerHandle ──────────────────────────────────────────────────────────────

/// Handle to the GLib worker thread.
pub(crate) struct WorkerHandle {
    pub cmd_tx: mpsc::SyncSender<Command>,
    pub engine_state: Arc<Mutex<EngineState>>,
    /// `Option` so [`Drop`] can take + join the handle. Stays `Some`
    /// until either `shutdown_and_join` is called explicitly or the
    /// engine is dropped.
    thread: Option<thread::JoinHandle<()>>,
}

impl WorkerHandle {
    /// Send Command::Shutdown to the worker, then block until its GLib
    /// main loop exits and the thread joins. Idempotent: a second call
    /// returns immediately because the JoinHandle is already taken.
    ///
    /// Must be called before `std::process::exit` so WebKit's atexit
    /// destructors don't WTFCrash unwinding a half-initialised state.
    pub(crate) fn shutdown_and_join(&mut self) {
        let Some(handle) = self.thread.take() else {
            return;
        };
        // Best-effort: if the worker queue is full or already gone, the
        // worker has crashed; we still try to join below.
        let _ = self.cmd_tx.try_send(Command::Shutdown);
        if let Err(e) = handle.join() {
            tracing::warn!(?e, "webkit worker: thread panicked during shutdown");
        }
    }
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        self.shutdown_and_join();
    }
}

/// Spawn the GLib worker thread and return a handle.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn(
    initial_url: &str,
    width: u32,
    height: u32,
    frame: SharedOsrFrame,
    view: SharedOsrViewState,
    is_loading_atomic: Arc<std::sync::atomic::AtomicBool>,
) -> Result<WorkerHandle, WebKitError> {
    // No FDO bootstrap on the new wpe-platform path — the BuffrDisplay
    // subclass owns its own EGL display and view lifecycle. `wpe_loader_init`
    // / `wpe_fdo_initialize_*` are gone with the Phase 2 scaffold.

    let engine_state = Arc::new(Mutex::new(EngineState::new(width, height)));
    let (cmd_tx, cmd_rx) = mpsc::sync_channel::<Command>(64);

    let initial_url = initial_url.to_owned();
    let es = Arc::clone(&engine_state);

    let thread = thread::Builder::new()
        .name("buffr-webkit-worker".into())
        .spawn(move || {
            tracing::info!("webkit worker: starting GLib main loop");

            // EGL display + GLES context, bound to this worker thread.
            // BuffrDisplay hands the raw EGLDisplay to WebKit via its
            // get_egl_display vmethod.
            let egl = match super::egl::EglWorker::new() {
                Ok(e) => e,
                Err(e) => {
                    tracing::error!("webkit worker: EGL init failed: {e}");
                    return;
                }
            };
            if let Err(e) = egl.make_current() {
                tracing::error!("webkit worker: eglMakeCurrent failed: {e}");
                return;
            }
            tracing::info!("webkit worker: EGL ready");

            // Acquire the default GMainContext on this thread (the app's
            // main thread never pumps any GMainContext, so we own it).
            let main_context = glib::MainContext::default();
            let _ctx_guard = match main_context.acquire() {
                Ok(g) => g,
                Err(e) => {
                    tracing::error!("webkit worker: cannot acquire default GMainContext: {e}");
                    return;
                }
            };

            // Build the GLib main loop bound to the default context.
            let main_loop = glib::MainLoop::new(None, false);

            use std::cell::RefCell;
            use std::rc::Rc;

            let runtime = match WpeRuntime::new(
                Arc::clone(&frame),
                Arc::clone(&view),
                Arc::clone(&es),
                egl,
                Arc::clone(&is_loading_atomic),
            ) {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::error!("webkit worker: WpeRuntime::new failed: {e}");
                    return;
                }
            };
            let runtime_rc = Rc::new(RefCell::new(runtime));

            // ── Initial tab: open inside the main loop via idle callback ──
            //
            // WebKit WPE requires the GLib main loop to be running before
            // any WebView is created (it needs its internal Wayland display
            // compositor + D-Bus process manager to initialise). Using
            // idle_add_local ensures the main loop is pumping before
            // wpe_view_backend_exportable_fdo_create is called.
            {
                let rt = Rc::clone(&runtime_rc);
                glib::idle_add_local_once(move || {
                    if let Err(e) = rt.borrow_mut().open_tab(&initial_url, false) {
                        tracing::error!("webkit worker: initial open_tab failed: {e}");
                    }
                });
            }

            // ── Audio-activity poll (500 ms) ──────────────────────────────
            {
                let rt = Rc::clone(&runtime_rc);
                let es_audio = Arc::clone(&es);
                glib::timeout_add_local(Duration::from_millis(500), move || {
                    let any = rt.borrow().any_audio_active();
                    if let Ok(st) = es_audio.lock() {
                        st.audio_active.store(any, Ordering::Relaxed);
                    }
                    glib::ControlFlow::Continue
                });
            }

            // ── Command poll (10 ms) ─────────────────────────────────────
            {
                let ml = main_loop.clone();
                let rt = Rc::clone(&runtime_rc);
                glib::timeout_add_local(Duration::from_millis(10), move || {
                    loop {
                        match cmd_rx.try_recv() {
                            Ok(cmd) => {
                                if handle_command(cmd, &mut rt.borrow_mut(), &ml) {
                                    return glib::ControlFlow::Break;
                                }
                            }
                            Err(mpsc::TryRecvError::Empty) => {
                                return glib::ControlFlow::Continue;
                            }
                            Err(mpsc::TryRecvError::Disconnected) => {
                                tracing::info!("webkit worker: command channel closed");
                                ml.quit();
                                return glib::ControlFlow::Break;
                            }
                        }
                    }
                });
            }

            tracing::info!("webkit worker: entering GLib main loop");
            main_loop.run();
            tracing::info!("webkit worker: main loop exited");
        })
        .map_err(|e| WebKitError::InitFailed(format!("failed to spawn worker: {e}")))?;

    Ok(WorkerHandle {
        cmd_tx,
        engine_state,
        thread: Some(thread),
    })
}

// ── Command handler ───────────────────────────────────────────────────────────

/// Returns `true` if the main loop should exit.
fn handle_command(cmd: Command, rt: &mut WpeRuntime, ml: &glib::MainLoop) -> bool {
    match cmd {
        Command::OpenTab {
            url,
            reply,
            background,
        } => {
            let res = rt.open_tab(&url, background).map_err(|e| e.to_string());
            let _ = reply.try_send(res);
        }
        Command::Navigate { url } => {
            rt.navigate(&url);
        }
        Command::Resize { width, height } | Command::OsrResize { width, height } => {
            rt.resize(width, height);
        }
        Command::KeyEvent { ev } => {
            rt.dispatch_keyboard(ev.key_code, ev.pressed, ev.modifiers);
        }
        Command::MouseMove { x, y, modifiers } => {
            rt.dispatch_pointer_motion(x, y, translate_modifiers(modifiers));
        }
        Command::MouseClick {
            x,
            y,
            button,
            pressed,
            modifiers,
        } => {
            rt.dispatch_pointer_button(x, y, button, pressed, translate_modifiers(modifiers));
        }
        Command::MouseWheel {
            x,
            y,
            delta_x,
            delta_y,
            modifiers,
        } => {
            rt.dispatch_axis(x, y, delta_x, delta_y, translate_modifiers(modifiers));
        }
        Command::Focus { focused: _ } => {
            // Focus tracking moves to the BuffrView focus_in/focus_out API.
            // Stub: no-op until the platform path is wired.
        }
        Command::OsrSleep { sleep } => {
            if let Ok(st) = rt.engine_state.lock() {
                st.osr_sleeping.store(sleep, Ordering::Relaxed);
            }
        }
        Command::CloseActive { reply } => {
            let closed = rt.close_active();
            let _ = reply.try_send(closed);
        }
        Command::CloseTab { id, reply } => {
            let closed = rt.close_tab(id);
            let _ = reply.try_send(closed);
        }
        Command::SelectTab { id } => {
            rt.select_tab(id);
        }
        Command::EvalJs { script } => {
            rt.eval_js(&script);
        }
        Command::Shutdown => {
            ml.quit();
            return true;
        }
    }
    false
}

/// Map CEF EVENTFLAG_* bitmask → WPE `wpe_input_modifier`.
///
/// CEF flags (from `cef_event_flags_t`):
///   SHIFT=1<<1=2, CONTROL=1<<2=4, ALT=1<<3=8, META=1<<5=32
/// WPE modifiers:
///   control=1, shift=2, alt=4, meta=8
fn translate_modifiers(cef: u32) -> u32 {
    let mut wpe = 0u32;
    // EVENTFLAG_SHIFT_DOWN = 0x02
    if cef & 0x02 != 0 {
        wpe |= 2; // wpe shift
    }
    // EVENTFLAG_CONTROL_DOWN = 0x04
    if cef & 0x04 != 0 {
        wpe |= 1; // wpe control
    }
    // EVENTFLAG_ALT_DOWN = 0x08
    if cef & 0x08 != 0 {
        wpe |= 4; // wpe alt
    }
    // EVENTFLAG_COMMAND_DOWN = 0x20
    if cef & 0x20 != 0 {
        wpe |= 8; // wpe meta
    }
    wpe
}
