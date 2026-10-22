//! Input injection through the GNOME remote desktop session.

use std::{collections::HashSet, sync::Arc};

use anyhow::Result;
use oriel_proto::{InputEvent, MouseButton, keys};

use super::Injector;
use crate::{
    capture::Geometry,
    gnome::{GnomeSession, InputCmd},
};

// evdev button codes.
const BTN_LEFT: i32 = 0x110;
const BTN_RIGHT: i32 = 0x111;
const BTN_MIDDLE: i32 = 0x112;
const BTN_SIDE: i32 = 0x113;
const BTN_EXTRA: i32 = 0x114;

pub struct GnomeInjector {
    session: Arc<GnomeSession>,
    keys: HashSet<u32>,
    buttons: HashSet<i32>,
    wheel: (i32, i32),
}

impl GnomeInjector {
    pub fn new(display: Option<u32>) -> Result<Self> {
        Ok(Self {
            session: GnomeSession::shared(display)?,
            keys: HashSet::new(),
            buttons: HashSet::new(),
            wheel: (0, 0),
        })
    }
}

impl Injector for GnomeInjector {
    fn inject(&mut self, event: InputEvent, _geometry: Geometry) -> Result<()> {
        match event {
            // Coordinates are in stream (surface) pixels already.
            InputEvent::MouseMove { x, y, .. } => self
                .session
                .input(InputCmd::MoveAbsolute(x as f64, y as f64)),
            InputEvent::MouseMotion { dx, dy } => self
                .session
                .input(InputCmd::MoveRelative(dx as f64, dy as f64)),
            InputEvent::MouseButton { button, down } => {
                let code = match button {
                    MouseButton::Left => BTN_LEFT,
                    MouseButton::Right => BTN_RIGHT,
                    MouseButton::Middle => BTN_MIDDLE,
                    MouseButton::Back => BTN_SIDE,
                    MouseButton::Forward => BTN_EXTRA,
                };
                if down {
                    self.buttons.insert(code);
                } else {
                    self.buttons.remove(&code);
                }
                self.session.input(InputCmd::Button(code, down));
            }
            InputEvent::MouseWheel { dx, dy } => {
                // Whole notches; Wayland's positive axis is down/right,
                // Windows' positive wheel is up.
                self.wheel.0 += dx;
                self.wheel.1 += dy;
                let (sx, sy) = (self.wheel.0 / 120, self.wheel.1 / 120);
                self.wheel.0 -= sx * 120;
                self.wheel.1 -= sy * 120;
                if sy != 0 {
                    self.session.input(InputCmd::Scroll {
                        axis: 0,
                        steps: -sy,
                    });
                }
                if sx != 0 {
                    self.session.input(InputCmd::Scroll { axis: 1, steps: sx });
                }
            }
            InputEvent::Key { usage, down } => {
                if let Some(code) = keys::hid_to_evdev(usage) {
                    let code = code as u32;
                    if down {
                        self.keys.insert(code);
                    } else {
                        self.keys.remove(&code);
                    }
                    self.session.input(InputCmd::Key(code, down));
                }
            }
            InputEvent::ReleaseAll => {
                for code in self.keys.drain() {
                    self.session.input(InputCmd::Key(code, false));
                }
                for code in self.buttons.drain() {
                    self.session.input(InputCmd::Button(code, false));
                }
            }
            InputEvent::SecureAttention => {}
        }
        Ok(())
    }
}
