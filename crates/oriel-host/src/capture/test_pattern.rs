//! A synthetic source for developing and testing without a real desktop:
//! an animated pattern that also draws the input it receives (pointer,
//! clicks, keys). F9 toggles a "game has captured the mouse" state.

use std::time::{Duration, Instant};

use anyhow::Result;
use oriel_proto::{CursorShape, DisplayInfo, keys};

use super::{Capturer, Geometry, TestState};
use crate::cursor::{self, CursorPos};

pub const WIDTH: u32 = 1920;
pub const HEIGHT: u32 = 1080;
const FPS: u32 = 60;

#[derive(Debug, Default)]
pub struct State {
    pub mouse: (i32, i32),
    pub buttons: u8,
    pub keys_down: Vec<u16>,
    pub captured: bool,
    pub wheel: i32,
    pub input_events: u64,
}

impl State {
    pub fn apply(&mut self, event: oriel_proto::InputEvent) {
        use oriel_proto::InputEvent::*;
        self.input_events += 1;
        let clamp = |v: i32, max: u32| v.clamp(0, max as i32 - 1);
        match event {
            MouseMove { x, y, .. } => self.mouse = (clamp(x, WIDTH), clamp(y, HEIGHT)),
            MouseMotion { dx, dy } => {
                self.mouse = (clamp(self.mouse.0 + dx, WIDTH), clamp(self.mouse.1 + dy, HEIGHT))
            }
            MouseButton { button, down } => {
                let bit = 1 << button as u8;
                if down {
                    self.buttons |= bit
                } else {
                    self.buttons &= !bit
                }
            }
            MouseWheel { dy, .. } => self.wheel += dy,
            Key { usage, down } => {
                if down && !self.keys_down.contains(&usage) {
                    self.keys_down.push(usage);
                    if usage == keys::F1 + 8 {
                        // F9
                        self.captured = !self.captured;
                    }
                } else if !down {
                    self.keys_down.retain(|k| *k != usage);
                }
            }
            ReleaseAll => {
                self.buttons = 0;
                self.keys_down.clear();
            }
            SecureAttention => {}
        }
    }
}

pub fn display() -> DisplayInfo {
    DisplayInfo {
        id: 0,
        name: "Test pattern".into(),
        width: WIDTH,
        height: HEIGHT,
        refresh_hz: FPS,
        primary: true,
    }
}

pub struct TestPattern {
    state: TestState,
    pixels: Vec<u32>,
    start: Instant,
    next: Instant,
    shape_sent: bool,
}

impl TestPattern {
    pub fn new(state: TestState) -> Self {
        Self {
            state,
            pixels: vec![0; (WIDTH * HEIGHT) as usize],
            start: Instant::now(),
            next: Instant::now(),
            shape_sent: false,
        }
    }

    fn render(&mut self) {
        let t = self.start.elapsed().as_secs_f32();
        let s = self.state.lock();
        let (w, h) = (WIDTH as usize, HEIGHT as usize);
        let shift = (t * 120.0) as usize;
        for y in 0..h {
            let row = &mut self.pixels[y * w..(y + 1) * w];
            let g = (y * 255 / h) as u32;
            for (x, px) in row.iter_mut().enumerate() {
                let r = (((x + shift) % w) * 255 / w) as u32;
                let grid = if x % 120 == 0 || y % 120 == 0 { 0x40 } else { 0 };
                *px = 0xFF00_0000 | ((r + grid).min(255) << 16) | ((g + grid).min(255) << 8) | 0x60;
            }
        }
        // A bar sweeping once per second makes dropped frames visible.
        let bar_x = ((t.fract()) * w as f32) as usize;
        fill(&mut self.pixels, bar_x.saturating_sub(8), 0, 16, h, 0xFFFF_FFFF);
        // Pressed keys light up a row of squares.
        for (i, _) in s.keys_down.iter().enumerate() {
            fill(&mut self.pixels, 40 + i * 50, 40, 40, 40, 0xFFFF_D000);
        }
        // Buttons: left/right/middle squares.
        for b in 0..3 {
            let color = if s.buttons & (1 << b) != 0 { 0xFF00_FF00 } else { 0xFF30_3030 };
            fill(&mut self.pixels, 40 + b * 50, 100, 40, 40, color);
        }
        if s.captured {
            fill(&mut self.pixels, 40, 160, 190, 20, 0xFFFF_0000);
        }
        // Crosshair where the host thinks the pointer is.
        let (mx, my) = (s.mouse.0 as usize, s.mouse.1 as usize);
        fill(&mut self.pixels, mx.saturating_sub(20), my, 41, 1, 0xFFFF_00FF);
        fill(&mut self.pixels, mx, my.saturating_sub(20), 1, 41, 0xFFFF_00FF);
    }
}

fn fill(px: &mut [u32], x: usize, y: usize, w: usize, h: usize, color: u32) {
    let (fw, fh) = (WIDTH as usize, HEIGHT as usize);
    for row in y.min(fh)..(y + h).min(fh) {
        let start = row * fw + x.min(fw);
        let end = row * fw + (x + w).min(fw);
        px[start..end].fill(color);
    }
}

impl Capturer for TestPattern {
    fn size(&self) -> (u32, u32) {
        (WIDTH, HEIGHT)
    }

    fn geometry(&self) -> Geometry {
        Geometry {
            left: 0,
            top: 0,
            width: WIDTH,
            height: HEIGHT,
        }
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<bool> {
        let now = Instant::now();
        if self.next > now {
            let wait = self.next - now;
            if wait > timeout {
                std::thread::sleep(timeout);
                return Ok(false);
            }
            std::thread::sleep(wait);
        }
        self.next = self.next.max(now - Duration::from_secs(1)) + Duration::from_secs(1) / FPS;
        self.render();
        Ok(true)
    }

    fn cpu_frame(&mut self) -> Result<(&[u8], usize)> {
        let bytes = unsafe {
            std::slice::from_raw_parts(self.pixels.as_ptr() as *const u8, self.pixels.len() * 4)
        };
        Ok((bytes, WIDTH as usize * 4))
    }

    fn cursor(&mut self) -> CursorPos {
        let s = self.state.lock();
        CursorPos {
            visible: !s.captured,
            captured: s.captured,
            x: s.mouse.0,
            y: s.mouse.1,
        }
    }

    fn cursor_shape(&mut self) -> Option<CursorShape> {
        if self.shape_sent {
            return None;
        }
        self.shape_sent = true;
        Some(cursor::arrow())
    }
}
