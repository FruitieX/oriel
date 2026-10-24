//! Input injection through the GNOME remote desktop session.

use std::collections::HashSet;

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

/// Injects into the session the capturer runs (see [`GnomeSession::current`]).
pub struct GnomeInjector {
    keys: HashSet<u32>,
    buttons: HashSet<i32>,
    wheel: (i32, i32),
}

impl GnomeInjector {
    pub fn new() -> Result<Self> {
        Ok(Self {
            keys: HashSet::new(),
            buttons: HashSet::new(),
            wheel: (0, 0),
        })
    }
}

/// Input goes to whichever session runs now (the display may have
/// changed since this injector was made).
fn input(cmd: InputCmd) {
    if let Some(session) = GnomeSession::current() {
        session.input(cmd);
    }
}

impl Injector for GnomeInjector {
    fn inject(&mut self, event: InputEvent, _geometry: Geometry) -> Result<()> {
        match event {
            // Coordinates are in stream (surface) pixels already.
            InputEvent::MouseMove { x, y, .. } => input(InputCmd::MoveAbsolute(x as f64, y as f64)),
            InputEvent::MouseMotion { dx, dy } => {
                input(InputCmd::MoveRelative(dx as f64, dy as f64))
            }
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
                input(InputCmd::Button(code, down));
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
                    input(InputCmd::Scroll {
                        axis: 0,
                        steps: -sy,
                    });
                }
                if sx != 0 {
                    input(InputCmd::Scroll { axis: 1, steps: sx });
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
                    input(InputCmd::Key(code, down));
                }
            }
            InputEvent::ReleaseAll => {
                for code in self.keys.drain() {
                    input(InputCmd::Key(code, false));
                }
                for code in self.buttons.drain() {
                    input(InputCmd::Button(code, false));
                }
            }
            InputEvent::SecureAttention => {}
            // The compositor repeats held keys itself.
            InputEvent::KeysHeld { .. } => {}
            InputEvent::Text { text } => {
                for c in text.chars() {
                    let sym = keysym(c);
                    input(InputCmd::Keysym(sym, true));
                    input(InputCmd::Keysym(sym, false));
                }
            }
        }
        Ok(())
    }
}

/// The X keysym that types `c`: Latin-1 directly, controls as their keys,
/// everything else as a Unicode keysym.
fn keysym(c: char) -> u32 {
    match c {
        '\n' | '\r' => 0xff0d, // Return
        '\t' => 0xff09,        // Tab
        '\u{8}' => 0xff08,     // BackSpace
        ' '..='~' | '\u{a0}'..='\u{ff}' => c as u32,
        _ => 0x0100_0000 + c as u32,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn keysyms() {
        assert_eq!(super::keysym('a'), 0x61);
        assert_eq!(super::keysym('ä'), 0xe4);
        assert_eq!(super::keysym('€'), 0x0100_20ac);
        assert_eq!(super::keysym('\n'), 0xff0d);
    }
}
