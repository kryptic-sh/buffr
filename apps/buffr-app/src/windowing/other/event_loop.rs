//! Event loop driver + [`ApplicationHandler`] trait.
//!
//! Wraps `winit::event_loop::EventLoop` and adapts winit's
//! `ApplicationHandler` shape to the wayr one so the buffr-app code
//! reads the same on both backends.
//!
//! # Lifetime trickery
//!
//! wayr's `ApplicationHandler` takes `&mut EventLoop<T>` in every
//! callback. winit's `ApplicationHandler` takes `&ActiveEventLoop`
//! (immutable; the platform layer owns the lifetime). We bridge by
//! storing the borrowed `*const ActiveEventLoop` inside our own
//! `EventLoop<T>` for the duration of the callback, then clearing
//! it on exit. The pointer is only ever dereferenced from within
//! the dispatch closure that owns the borrow, so it can't outlive
//! the borrow.

use std::collections::HashMap;
use std::num::NonZeroU64;
use std::sync::Arc;
use std::time::Instant;

use winit::application::ApplicationHandler as WinitAppHandler;
use winit::event::WindowEvent as WinitWindowEvent;
use winit::event_loop::ActiveEventLoop;
use winit::window::WindowId as WinitWindowId;

use super::cursor::CursorIcon;
use super::event::WindowEvent;
use super::geometry::{Position, Size};
use super::ime::ImeEvent;
use super::keyboard::{
    KeyEvent, KeyState, Modifiers, key_code_from_winit_logical, modifiers_from_winit,
    scancode_from_winit_physical,
};
use super::output::{OutputId, OutputInfo};
use super::pointer::{
    AxisDirection, AxisSource, PointerButton, PointerButtonState, PointerPosition, ScrollEvent,
};
use super::surface::SurfaceId;
use super::window::{BuildError, ToplevelBuilder, Window};

/// Application-side hook called by the event loop. Mirrors wayr's
/// shape (which mirrors winit's, modulo the user-event generic and
/// the `&mut` borrow).
pub trait ApplicationHandler<T = ()> {
    /// Called once after [`EventLoop::run_app`] starts.
    fn resumed(&mut self, _event_loop: &mut EventLoop<T>) {}

    /// Per-surface event.
    fn window_event(
        &mut self,
        _event_loop: &mut EventLoop<T>,
        _surface_id: SurfaceId,
        _event: WindowEvent,
    ) {
    }

    /// User event dispatched via [`EventLoopProxy::send_event`].
    fn user_event(&mut self, _event_loop: &mut EventLoop<T>, _event: T) {}

    /// Loop is about to block waiting for new events.
    fn about_to_wait(&mut self, _event_loop: &mut EventLoop<T>) {}

    /// Loop is shutting down. No further callbacks will fire.
    fn exiting(&mut self, _event_loop: &mut EventLoop<T>) {}
}

/// Error returned by [`EventLoopProxy::send_event`] when the loop has
/// already exited. Carries the original event so the caller can
/// recover the payload (matches both winit's `EventLoopClosed<T>` and
/// stdlib's `mpsc::SendError<T>` shape).
#[derive(Debug)]
pub struct SendError<T>(pub T);

impl<T> std::fmt::Display for SendError<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("event loop closed")
    }
}

impl<T: std::fmt::Debug> std::error::Error for SendError<T> {}

/// Send-able / clone-able handle for dispatching user events into the
/// loop from other threads.
pub struct EventLoopProxy<T: 'static> {
    inner: winit::event_loop::EventLoopProxy<T>,
}

impl<T: 'static> EventLoopProxy<T> {
    /// Send a user event.
    pub fn send_event(&self, event: T) -> Result<(), SendError<T>> {
        self.inner.send_event(event).map_err(|e| SendError(e.0))
    }
}

impl<T: 'static> Clone for EventLoopProxy<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

/// Event loop owning the winit driver + bookkeeping for our
/// surface-id allocation, cached modifier state, and pending
/// builder requests.
pub struct EventLoop<T: 'static> {
    /// Owned winit event loop. Consumed by `run_app` (winit's
    /// `EventLoop::run_app` takes `self`).
    inner: Option<winit::event_loop::EventLoop<T>>,
    /// Cached proxy — lets us hand out `EventLoopProxy<T>` clones
    /// without consuming `inner`.
    proxy: winit::event_loop::EventLoopProxy<T>,
    /// Borrowed `&ActiveEventLoop`, live only during a dispatch
    /// callback. Stored as a raw pointer so the same `&mut EventLoop`
    /// can be threaded through every `ApplicationHandler` method
    /// without crashing winit's borrow-checker. SAFETY contract: the
    /// pointer is set to non-null only inside a winit callback (where
    /// the underlying `ActiveEventLoop` is provably borrowed for the
    /// duration of the call) and cleared before the callback returns.
    active: Option<*const ActiveEventLoop>,
    /// Allocator for `SurfaceId`s (winit's `WindowId` is u64 but with
    /// no nonzero guarantee, so we use our own counter).
    next_surface_id: u64,
    /// Map winit's `WindowId` to our `SurfaceId`. Populated by
    /// [`Self::build_window`] and consumed by the dispatch path.
    id_map: HashMap<WinitWindowId, SurfaceId>,
    /// Cached modifier state — attached to `PointerButton` and `Key`
    /// events that winit fires without the modifiers inline.
    modifiers: winit::keyboard::ModifiersState,
    /// Last seen cursor position per surface. winit's
    /// `CursorEntered` / `CursorLeft` events carry no position, so
    /// we synthesise the bridge `PointerEntered { position }` from
    /// the most recent move.
    last_cursor_pos_per_window: HashMap<WinitWindowId, winit::dpi::PhysicalPosition<f64>>,
    /// Pending exit flag set by [`Self::exit`] — applied to the
    /// winit `ActiveEventLoop` at the next opportunity.
    exit_requested: bool,
    /// Cached `wait_until` deadline. Applied to winit's
    /// `set_control_flow(Wait | WaitUntil)` from inside the dispatch
    /// callback. Single-shot — matches wayr.
    wait_until: Option<Instant>,
    /// Cached cursor request from `set_cursor` calls made between
    /// dispatch callbacks; applied to whichever window currently
    /// holds pointer focus.
    pending_cursor: Option<CursorIcon>,
    /// Currently focused window — receives `pending_cursor`.
    focused_window: Option<WinitWindowId>,
    /// Stash any `Window`s created via the builder during a
    /// callback. The callback returns the `Window` to the caller
    /// once the borrow ends.
    _phantom: std::marker::PhantomData<T>,
}

impl<T: 'static> EventLoop<T> {
    /// Construct the event loop.
    pub fn new() -> Result<Self, BuildError> {
        let inner = winit::event_loop::EventLoop::<T>::with_user_event()
            .build()
            .map_err(|e| BuildError(e.to_string()))?;
        let proxy = inner.create_proxy();
        Ok(Self {
            inner: Some(inner),
            proxy,
            active: None,
            next_surface_id: 1,
            id_map: HashMap::new(),
            modifiers: winit::keyboard::ModifiersState::default(),
            last_cursor_pos_per_window: HashMap::new(),
            exit_requested: false,
            wait_until: None,
            pending_cursor: None,
            focused_window: None,
            _phantom: std::marker::PhantomData,
        })
    }

    /// Cheap proxy handle for sending user events from other threads.
    pub fn proxy(&self) -> EventLoopProxy<T> {
        EventLoopProxy {
            inner: self.proxy.clone(),
        }
    }

    /// Request the loop exit after the current iteration.
    pub fn exit(&mut self) {
        self.exit_requested = true;
        if let Some(p) = self.active {
            // SAFETY: `active` is non-null only inside a winit
            // dispatch callback, where the borrow on the underlying
            // ActiveEventLoop is live (see Self::with_active).
            unsafe { (*p).exit() };
        }
    }

    /// Cap the next sleep at `deadline`. Single-shot.
    pub fn wait_until(&mut self, deadline: Instant) {
        self.wait_until = Some(match self.wait_until {
            Some(prev) => prev.min(deadline),
            None => deadline,
        });
    }

    /// Snapshot of every connected monitor. Maps winit's
    /// `available_monitors()` into the wayr-shaped `OutputInfo`.
    pub fn outputs(&self) -> Vec<OutputInfo> {
        let Some(p) = self.active else {
            // Outside a callback we can't query winit. wayr's outputs()
            // is also driven by dispatch state, so an empty result is
            // benign — main.rs falls back to a 60 Hz default.
            return Vec::new();
        };
        // SAFETY: as Self::exit above.
        let active = unsafe { &*p };
        let mut out = Vec::new();
        for (idx, monitor) in active.available_monitors().enumerate() {
            let size = monitor.size();
            let pos = monitor.position();
            let scale = monitor.scale_factor().round() as i32;
            let refresh_mhz = monitor.refresh_rate_millihertz().unwrap_or(0) as i32;
            out.push(OutputInfo {
                id: OutputId(idx as u64 + 1),
                name: monitor.name(),
                description: None,
                scale: scale.max(1),
                physical_size: Size::new(size.width, size.height),
                position: (pos.x, pos.y),
                refresh_mhz,
            });
        }
        out
    }

    /// Set the cursor shape shown over the focused window.
    pub fn set_cursor(&self, icon: CursorIcon) {
        // We can't directly call `set_cursor` here because we don't
        // hold a `Window` reference. Stash the request and apply it
        // on the next dispatch via the focused-window cache below.
        // Use interior-mutability cell? Self is `&self`. wayr's
        // signature is also `&self` because the call routes through
        // shared compositor state. We piggyback on the focused-window
        // tracker — but it needs interior mutability. Wrap it in a
        // RefCell? Simpler: provide a `set_cursor(&mut self)` shape
        // diverges from wayr. Instead: walk every known winit window
        // (we keep an id_map) — and call set_cursor on each. winit
        // only honours the request for the focused window anyway.
        let _ = icon;
        // Implementation detail: we cannot call winit set_cursor
        // without an `Arc<winit::Window>` handle (the bridge `Window`
        // owns it). The caller path (`pump_cursor_changes` in
        // main.rs) holds the focused window via the `Window` wrapper
        // already — refactoring there is out of scope. For now this
        // is a no-op; the cursor matches the OS default. The active
        // wayr backend does the per-seat compositor call.
        //
        // TODO(windowing/other): expose a `set_cursor` path that
        // takes the focused `Arc<Window>` (the buffr-app code already
        // has it) so this stops being a no-op on macOS / Windows.
    }

    /// Construct a window via the builder. Called by
    /// [`ToplevelBuilder::build`].
    pub(super) fn build_window(&mut self, builder: ToplevelBuilder) -> Result<Window, BuildError> {
        let Some(p) = self.active else {
            return Err(BuildError(
                "window creation only valid inside an ApplicationHandler callback".into(),
            ));
        };
        // SAFETY: as Self::exit above.
        let active = unsafe { &*p };
        let mut attrs = winit::window::WindowAttributes::default();
        if let Some(title) = builder.title {
            attrs = attrs.with_title(title);
        }
        if let Some(s) = builder.initial_size {
            attrs = attrs.with_inner_size(winit::dpi::LogicalSize::new(s.width, s.height));
        }
        if let Some(s) = builder.min_size {
            attrs = attrs.with_min_inner_size(winit::dpi::LogicalSize::new(s.width, s.height));
        }
        if let Some(s) = builder.max_size {
            attrs = attrs.with_max_inner_size(winit::dpi::LogicalSize::new(s.width, s.height));
        }
        // app_id intentionally dropped — winit has no portable equivalent.
        let win = active
            .create_window(attrs)
            .map_err(|e| BuildError(e.to_string()))?;
        // Bootstrap the first paint. wayr fires its own initial
        // `WindowEvent::RedrawRequested` after the empty commit that
        // kicks off the configure cycle, but winit only fires
        // `RedrawRequested` when the OS issues `WM_PAINT` (or we ask
        // for one). On headless / non-interactive Windows sessions
        // (e.g. CI runners with no logged-in user) `WM_PAINT` never
        // arrives. Asking explicitly here matches wayr's behaviour
        // and lets the buffr-app dispatch loop reach steady state on
        // every platform.
        win.request_redraw();
        let raw_id = win.id();
        let nz = NonZeroU64::new(self.next_surface_id)
            .expect("surface-id counter starts at 1 and only increments");
        self.next_surface_id = self
            .next_surface_id
            .checked_add(1)
            .expect("surface-id counter overflow");
        let sid = SurfaceId::from_nonzero(nz);
        self.id_map.insert(raw_id, sid);
        Ok(Window {
            id: sid,
            inner: Arc::new(win),
        })
    }

    /// Apply the cached `wait_until` to the winit control flow.
    fn apply_control_flow(&mut self, active: &ActiveEventLoop) {
        match self.wait_until.take() {
            Some(deadline) => {
                active.set_control_flow(winit::event_loop::ControlFlow::WaitUntil(deadline))
            }
            None => active.set_control_flow(winit::event_loop::ControlFlow::Wait),
        }
    }

    /// Run the event loop blocking. Returns when [`Self::exit`] is
    /// called.
    pub fn run_app<A>(mut self, app: &mut A) -> Result<(), BuildError>
    where
        A: ApplicationHandler<T>,
    {
        let inner = self
            .inner
            .take()
            .expect("EventLoop::run_app called more than once");
        let mut bridge = Bridge { ev: &mut self, app };
        inner
            .run_app(&mut bridge)
            .map_err(|e| BuildError(e.to_string()))
    }
}

// ── winit dispatch bridge ────────────────────────────────────────────────────

/// Internal adapter that implements winit's `ApplicationHandler`
/// and forwards into the user's bridge `ApplicationHandler`.
///
/// Holds a mutable borrow of the bridge `EventLoop<T>` and the
/// user's handler; the user only sees `&mut EventLoop<T>` (the
/// `ActiveEventLoop` is stashed into `ev.active` for the duration
/// of each callback).
struct Bridge<'a, T: 'static, A: ApplicationHandler<T>> {
    ev: &'a mut EventLoop<T>,
    app: &'a mut A,
}

impl<'a, T: 'static, A: ApplicationHandler<T>> Bridge<'a, T, A> {
    /// Run `f` with the bridge's `EventLoop<T>` configured so any
    /// `ApplicationHandler` callback that consults it sees the
    /// current `ActiveEventLoop`.
    fn with_active<R>(&mut self, active: &ActiveEventLoop, f: impl FnOnce(&mut Self) -> R) -> R {
        // Stash the active loop as a raw pointer (lifetime erased).
        // SAFETY: `active` is a `&ActiveEventLoop` borrow that lives
        // at least until `f` returns; we clear `ev.active` before
        // returning so no later code dereferences a dangling pointer.
        self.ev.active = Some(active as *const _);
        let r = f(self);
        self.ev.active = None;
        // Apply any deferred state.
        if self.ev.exit_requested {
            active.exit();
        }
        // Always re-issue control flow because winit defaults to
        // Poll between callbacks — without this the loop spins on
        // CPU even when nothing is happening.
        self.ev.apply_control_flow(active);
        r
    }
}

impl<'a, T: 'static, A: ApplicationHandler<T>> WinitAppHandler<T> for Bridge<'a, T, A> {
    fn resumed(&mut self, active: &ActiveEventLoop) {
        self.with_active(active, |b| b.app.resumed(b.ev));
    }

    fn user_event(&mut self, active: &ActiveEventLoop, event: T) {
        self.with_active(active, |b| b.app.user_event(b.ev, event));
    }

    fn about_to_wait(&mut self, active: &ActiveEventLoop) {
        self.with_active(active, |b| b.app.about_to_wait(b.ev));
    }

    fn exiting(&mut self, active: &ActiveEventLoop) {
        self.with_active(active, |b| b.app.exiting(b.ev));
    }

    fn window_event(
        &mut self,
        active: &ActiveEventLoop,
        window_id: WinitWindowId,
        event: WinitWindowEvent,
    ) {
        // ModifiersChanged is consumed inline — we cache the state so
        // subsequent PointerButton / Key events can attach it (wayr
        // bakes modifiers into each event).
        if let WinitWindowEvent::ModifiersChanged(mods) = &event {
            self.ev.modifiers = mods.state();
            return;
        }
        // Track cursor position so PointerEntered carries one too.
        if let WinitWindowEvent::CursorMoved { position, .. } = &event {
            self.ev
                .last_cursor_pos_per_window
                .insert(window_id, *position);
        }
        // Track focused window for cursor routing.
        if let WinitWindowEvent::Focused(true) = &event {
            self.ev.focused_window = Some(window_id);
        }
        if let WinitWindowEvent::Focused(false) = &event
            && self.ev.focused_window == Some(window_id)
        {
            self.ev.focused_window = None;
        }

        // Resolve our SurfaceId. If we haven't seen this winit
        // WindowId before, drop the event — it can't possibly belong
        // to a buffr-app surface.
        let Some(&sid) = self.ev.id_map.get(&window_id) else {
            return;
        };

        // Map winit's WindowEvent → bridge WindowEvent.
        let bridge_event = match event {
            WinitWindowEvent::Resized(s) => {
                Some(WindowEvent::Resized(Size::new(s.width, s.height)))
            }
            WinitWindowEvent::CloseRequested => Some(WindowEvent::CloseRequested),
            WinitWindowEvent::RedrawRequested => Some(WindowEvent::RedrawRequested),
            WinitWindowEvent::Focused(true) => Some(WindowEvent::Focused),
            WinitWindowEvent::Focused(false) => Some(WindowEvent::Unfocused),
            WinitWindowEvent::Occluded(occ) => Some(WindowEvent::Occluded(occ)),
            WinitWindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                // winit's InnerSizeWriter is opaque; "suggested" size
                // is approximated as the current physical size since
                // we can't read the writer without applying.
                let phys = self
                    .ev
                    .id_map
                    .iter()
                    .find(|(_wid, sid_)| **sid_ == sid)
                    .map(|(_, _)| ())
                    .and(None::<Size>) // placeholder — winit doesn't expose size pre-write
                    .unwrap_or_default();
                Some(WindowEvent::ScaleFactorChanged {
                    new_scale_factor: scale_factor,
                    suggested_size: phys,
                })
            }
            WinitWindowEvent::CursorEntered { .. } => {
                let pos = self
                    .ev
                    .last_cursor_pos_per_window
                    .get(&window_id)
                    .copied()
                    .unwrap_or(winit::dpi::PhysicalPosition::new(0.0, 0.0));
                Some(WindowEvent::PointerEntered {
                    position: PointerPosition(Position::new(
                        pos.x.round() as i32,
                        pos.y.round() as i32,
                    )),
                })
            }
            WinitWindowEvent::CursorLeft { .. } => Some(WindowEvent::PointerLeft),
            WinitWindowEvent::CursorMoved { position, .. } => Some(WindowEvent::PointerMoved {
                position: PointerPosition(Position::new(
                    position.x.round() as i32,
                    position.y.round() as i32,
                )),
            }),
            WinitWindowEvent::MouseInput { state, button, .. } => {
                let mods = modifiers_from_winit(self.ev.modifiers);
                Some(WindowEvent::PointerButton {
                    button: mouse_button_from_winit(button),
                    state: match state {
                        winit::event::ElementState::Pressed => PointerButtonState::Pressed,
                        winit::event::ElementState::Released => PointerButtonState::Released,
                    },
                    modifiers: mods,
                })
            }
            WinitWindowEvent::MouseWheel { delta, .. } => {
                Some(WindowEvent::Scroll(scroll_event_from_winit(delta)))
            }
            WinitWindowEvent::KeyboardInput { event: ke, .. } => {
                let modifiers = modifiers_from_winit(self.ev.modifiers);
                let bridge_key = bridge_key_event(ke, modifiers);
                Some(WindowEvent::Key(bridge_key))
            }
            WinitWindowEvent::Ime(ime) => match ime {
                winit::event::Ime::Preedit(text, cursor) => {
                    Some(WindowEvent::Ime(ImeEvent::Preedit {
                        text,
                        // winit gives a byte range (start, end); take
                        // start as the caret. wayr's `cursor` field
                        // is a single offset.
                        cursor: cursor.map(|(start, _end)| start as u32),
                    }))
                }
                winit::event::Ime::Commit(text) => Some(WindowEvent::Ime(ImeEvent::Commit(text))),
                // Enabled / Disabled have no wayr equivalent — wayr's
                // IME lifecycle is consumer-driven via Ime::enable().
                winit::event::Ime::Enabled | winit::event::Ime::Disabled => None,
            },
            // Variants we deliberately drop — no wayr equivalent or
            // not used by buffr-app:
            //   - ModifiersChanged (handled above, cached)
            //   - Moved, Destroyed, DroppedFile, HoveredFile,
            //     HoveredFileCancelled
            //   - PinchGesture, PanGesture, DoubleTapGesture,
            //     RotationGesture, TouchpadPressure, AxisMotion,
            //     ActivationTokenDone, ThemeChanged
            //   - Touch (TODO: map when buffr-app needs it)
            _ => None,
        };

        let Some(bridge_event) = bridge_event else {
            return;
        };
        self.with_active(active, |b| b.app.window_event(b.ev, sid, bridge_event));
    }
}

fn mouse_button_from_winit(b: winit::event::MouseButton) -> PointerButton {
    match b {
        winit::event::MouseButton::Left => PointerButton::Left,
        winit::event::MouseButton::Right => PointerButton::Right,
        winit::event::MouseButton::Middle => PointerButton::Middle,
        winit::event::MouseButton::Back => PointerButton::Back,
        winit::event::MouseButton::Forward => PointerButton::Forward,
        winit::event::MouseButton::Other(n) => PointerButton::Other(n as u32),
    }
}

fn scroll_event_from_winit(delta: winit::event::MouseScrollDelta) -> ScrollEvent {
    match delta {
        winit::event::MouseScrollDelta::LineDelta(dx, dy) => {
            // Treat vertical first (matches CEF expectations); if
            // both axes nonzero, attribute to whichever is larger.
            let (axis, val_dy, val_dx) = if dy.abs() >= dx.abs() {
                (AxisDirection::Vertical, dy, dx)
            } else {
                (AxisDirection::Horizontal, dx, dy)
            };
            let delta_lines = if axis == AxisDirection::Vertical {
                val_dy as f64
            } else {
                val_dx as f64
            };
            // wayr expresses smooth delta in logical pixels. A common
            // convention is 1 line ≈ 20 logical pixels — match the
            // wayr-side conversion.
            ScrollEvent {
                axis,
                delta: delta_lines * 20.0,
                discrete_steps: delta_lines.round() as i32,
                high_res_120: (delta_lines * 120.0).round() as i32,
                source: AxisSource::Wheel,
            }
        }
        winit::event::MouseScrollDelta::PixelDelta(p) => {
            let (axis, val) = if p.y.abs() >= p.x.abs() {
                (AxisDirection::Vertical, p.y)
            } else {
                (AxisDirection::Horizontal, p.x)
            };
            ScrollEvent {
                axis,
                delta: val,
                discrete_steps: 0,
                high_res_120: 0,
                source: AxisSource::Finger,
            }
        }
    }
}

fn bridge_key_event(ke: winit::event::KeyEvent, modifiers: Modifiers) -> KeyEvent {
    let scancode = scancode_from_winit_physical(ke.physical_key);
    let key_code = key_code_from_winit_logical(&ke.logical_key);
    let text = ke.text.as_deref().map(|s| s.to_string()).filter(|s| {
        // wayr filters single ASCII controls. Match the filter.
        if s.chars().count() == 1 {
            let c = s.chars().next().unwrap();
            !c.is_ascii_control()
        } else {
            !s.is_empty()
        }
    });
    let state = match ke.state {
        winit::event::ElementState::Pressed => KeyState::Pressed,
        winit::event::ElementState::Released => KeyState::Released,
    };
    KeyEvent::new(scancode, key_code, modifiers, state, text, ke.repeat)
}
