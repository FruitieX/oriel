//! Unaccelerated mouse movement on macOS, for when the host has captured
//! the cursor. winit's relative motion there is `NSEvent`'s delta: the
//! cursor's movement after macOS's pointer acceleration, which then made
//! aiming in games feel wrong. Apple's GameController framework reports the
//! mouse's own movement (as games on macOS use), without any permission.
//! Mice it does not see (or other platforms) keep winit's motion.

use std::{cell::Cell, ffi::c_float, ptr::NonNull, rc::Rc, time::Instant};

use block2::RcBlock;
use objc2_game_controller::{GCMouse, GCMouseInput};

/// How long after a raw movement winit's (accelerated) one is ignored.
const RAW_RECENT: std::time::Duration = std::time::Duration::from_millis(250);

struct State {
    enabled: Cell<bool>,
    last: Cell<Option<Instant>>,
    /// Fractions of a count not sent yet.
    rest: Cell<(f64, f64)>,
    /// When handlers were last attached.
    attached: Cell<Option<Instant>>,
    send: Box<dyn Fn(i32, i32)>,
}

type Handler = RcBlock<dyn Fn(NonNull<GCMouseInput>, c_float, c_float)>;

pub struct RawMouse {
    state: Rc<State>,
    handler: Handler,
}

impl RawMouse {
    /// `send` gets whole counts (x right, y down). Main thread only.
    pub fn new(send: impl Fn(i32, i32) + 'static) -> Self {
        let state = Rc::new(State {
            enabled: Cell::new(false),
            last: Cell::new(None),
            rest: Cell::new((0.0, 0.0)),
            attached: Cell::new(None),
            send: Box::new(send),
        });
        // Mice are found asynchronously: start looking now.
        // SAFETY: a plain class query on the main thread.
        let _ = unsafe { GCMouse::mice() };
        let handler_state = state.clone();
        let handler = RcBlock::new(move |_: NonNull<GCMouseInput>, dx: c_float, dy: c_float| {
            let s = &handler_state;
            if !s.enabled.get() {
                return;
            }
            s.last.set(Some(Instant::now()));
            // GameController's y axis points up.
            let (rx, ry) = s.rest.get();
            let (x, y) = (rx + dx as f64, ry - dy as f64);
            let (ix, iy) = (x.trunc(), y.trunc());
            s.rest.set((x - ix, y - iy));
            if ix != 0.0 || iy != 0.0 {
                (s.send)(ix as i32, iy as i32);
            }
        });
        Self { state, handler }
    }

    /// Start (or stop) sending raw movement. Starting attaches to every
    /// mouse connected now.
    pub fn set_enabled(&self, on: bool) {
        self.state.enabled.set(on);
        self.state.rest.set((0.0, 0.0));
        if !on {
            self.state.last.set(None);
            return;
        }
        self.attach();
    }

    fn attach(&self) {
        self.state.attached.set(Some(Instant::now()));
        // SAFETY: GameController calls on the main thread; the handler is
        // copied by the setter and also kept alive by `self`.
        unsafe {
            for mouse in GCMouse::mice().iter() {
                if let Some(input) = mouse.mouseInput() {
                    input.setMouseMovedHandler(&*self.handler as *const _ as *mut _);
                }
            }
        }
    }

    /// The cursor moved (as winit saw it) without raw movement: attach to
    /// mice found since, at most once a second.
    pub fn moved(&self) {
        if self.state.enabled.get()
            && !self.active()
            && self
                .state
                .attached
                .get()
                .is_none_or(|t| t.elapsed() > std::time::Duration::from_secs(1))
        {
            self.attach();
        }
    }

    /// Raw movement arrived lately: winit's accelerated motion is the same
    /// movement and must not be sent too.
    pub fn active(&self) -> bool {
        self.state.enabled.get()
            && self
                .state
                .last
                .get()
                .is_some_and(|t| t.elapsed() < RAW_RECENT)
    }
}

impl Drop for RawMouse {
    fn drop(&mut self) {
        // SAFETY: as in `set_enabled`; a null handler removes ours.
        unsafe {
            for mouse in GCMouse::mice().iter() {
                if let Some(input) = mouse.mouseInput() {
                    input.setMouseMovedHandler(std::ptr::null_mut());
                }
            }
        }
    }
}
