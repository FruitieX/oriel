//! Android: input becomes accessibility gestures and actions (the app's
//! accessibility service performs them; see `Gestures`). A click is a tap,
//! a drag a swipe along the pointer's path, a right click a long press;
//! Escape is Back. Text goes into the focused field.

use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

use anyhow::Result;
use oriel_proto::{InputEvent, MouseButton, keys};

use super::Injector;
use crate::capture::Geometry;

/// What the app's accessibility service can do (implemented over JNI).
pub trait Gestures: Send + Sync {
    fn tap(&self, x: f32, y: f32, long: bool);
    /// A swipe along `path` (screen pixels) taking `ms`.
    fn swipe(&self, path: &[(f32, f32)], ms: u32);
    fn global(&self, action: Global);
    /// Type into the focused field.
    fn text(&self, text: &str);
    fn backspace(&self);
    fn enter(&self);
}

#[derive(Debug, Clone, Copy)]
pub enum Global {
    Back,
    Home,
    Recents,
}

static GESTURES: Mutex<Option<Arc<dyn Gestures>>> = Mutex::new(None);

pub fn set_gestures(g: Option<Arc<dyn Gestures>>) {
    *GESTURES.lock().unwrap() = g;
}

fn gestures() -> Option<Arc<dyn Gestures>> {
    GESTURES.lock().unwrap().clone()
}

#[derive(Default)]
pub struct AccessibilityInjector {
    pos: (f32, f32),
    /// Left button held: when, and the pointer's path since.
    drag: Option<(Instant, Vec<(f32, f32)>)>,
    shift: bool,
}

/// Moving less than this (pixels) is still a tap.
const SLOP: f32 = 12.0;

impl Injector for AccessibilityInjector {
    fn inject(&mut self, event: InputEvent, _geometry: Geometry) -> Result<()> {
        let Some(g) = gestures() else {
            return Ok(());
        };
        match event {
            InputEvent::MouseMove { x, y, .. } => {
                self.pos = (x as f32, y as f32);
                if let Some((_, path)) = &mut self.drag {
                    path.push(self.pos);
                }
            }
            InputEvent::MouseMotion { dx, dy } => {
                self.pos.0 += dx as f32;
                self.pos.1 += dy as f32;
                if let Some((_, path)) = &mut self.drag {
                    path.push(self.pos);
                }
            }
            InputEvent::MouseButton { button, down } => match (button, down) {
                (MouseButton::Left, true) => self.drag = Some((Instant::now(), vec![self.pos])),
                (MouseButton::Left, false) => {
                    if let Some((started, path)) = self.drag.take() {
                        let start = path[0];
                        let far = path
                            .iter()
                            .any(|p| (p.0 - start.0).hypot(p.1 - start.1) > SLOP);
                        let ms = started.elapsed().as_millis() as u32;
                        if !far {
                            g.tap(start.0, start.1, ms >= 600);
                        } else {
                            g.swipe(&path, ms.clamp(50, 5000));
                        }
                    }
                }
                (MouseButton::Right, false) => g.tap(self.pos.0, self.pos.1, true),
                (MouseButton::Back, false) => g.global(Global::Back),
                (MouseButton::Forward, false) => g.global(Global::Recents),
                _ => {}
            },
            InputEvent::MouseWheel { dy, dx } => {
                // A notch (120) scrolls about a fifth of a screen: swipe the
                // other way.
                let (x, y) = self.pos;
                let to = (x - dx as f32 * 2.5, y + dy as f32 * 2.5);
                g.swipe(&[(x, y), to], 150);
            }
            InputEvent::Key { usage, down } => {
                if usage == keys::SHIFT_LEFT || usage == keys::SHIFT_RIGHT {
                    self.shift = down;
                } else if down {
                    match usage {
                        keys::ESCAPE => g.global(Global::Back),
                        keys::BACKSPACE => g.backspace(),
                        keys::ENTER => g.enter(),
                        _ => {
                            if let Some(c) = us_char(usage, self.shift) {
                                g.text(&c.to_string());
                            }
                        }
                    }
                }
            }
            InputEvent::Text { text } => {
                for part in text.split_inclusive(['\n', '\u{8}']) {
                    let (body, end) = match part.chars().last() {
                        Some(c @ ('\n' | '\u{8}')) => (&part[..part.len() - 1], Some(c)),
                        _ => (part, None),
                    };
                    if !body.is_empty() {
                        g.text(body);
                    }
                    match end {
                        Some('\n') => g.enter(),
                        Some(_) => g.backspace(),
                        None => {}
                    }
                }
            }
            InputEvent::ReleaseAll => self.drag = None,
            InputEvent::SecureAttention => g.global(Global::Home),
        }
        Ok(())
    }
}

/// The character a key types on a US layout.
fn us_char(usage: u16, shift: bool) -> Option<char> {
    let (plain, shifted) = match usage {
        0x04..=0x1D => {
            let c = (b'a' + (usage - 0x04) as u8) as char;
            (c, c.to_ascii_uppercase())
        }
        0x1E..=0x26 => {
            let digit = (b'1' + (usage - 0x1E) as u8) as char;
            (digit, b"!@#$%^&*("[(usage - 0x1E) as usize] as char)
        }
        0x27 => ('0', ')'),
        0x2C => (' ', ' '),
        0x2D => ('-', '_'),
        0x2E => ('=', '+'),
        0x2F => ('[', '{'),
        0x30 => (']', '}'),
        0x31 => ('\\', '|'),
        0x33 => (';', ':'),
        0x34 => ('\'', '"'),
        0x35 => ('`', '~'),
        0x36 => (',', '<'),
        0x37 => ('.', '>'),
        0x38 => ('/', '?'),
        _ => return None,
    };
    Some(if shift { shifted } else { plain })
}

#[cfg(test)]
mod tests {
    #[test]
    fn us_layout() {
        assert_eq!(super::us_char(0x04, false), Some('a'));
        assert_eq!(super::us_char(0x04, true), Some('A'));
        assert_eq!(super::us_char(0x1E, true), Some('!'));
        assert_eq!(super::us_char(0x27, false), Some('0'));
    }
}
