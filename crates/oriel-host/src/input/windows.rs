//! Input injection with `SendInput`.

use std::{
    collections::HashSet,
    time::{Duration, Instant},
};

use anyhow::{Result, bail};
use oriel_proto::{InputEvent, MouseButton, keys};
use windows::Win32::UI::{
    Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT,
        KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE,
        MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
        MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
        MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL,
        MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, SendInput, VIRTUAL_KEY, VK_PAUSE,
    },
    WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN, SPI_GETKEYBOARDDELAY, SPI_GETKEYBOARDSPEED,
        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
    },
};

use super::Injector;
use crate::capture::Geometry;

const XBUTTON1: i32 = 1;
const XBUTTON2: i32 = 2;

pub struct SendInputInjector {
    keys: HashSet<u16>,
    buttons: HashSet<MouseButton>,
    /// Injected keys do not auto-repeat as a keyboard's do: the key held
    /// last, when it next repeats, and how often.
    repeat: Option<(u16, Instant, Duration)>,
    /// When the client last said which keys it holds; clients that never
    /// do (older ones) get repeats without the check.
    held_at: Option<Instant>,
}

/// Repeats pause when the client has not confirmed its held keys for this
/// long, and resume when it does.
const HELD_TIMEOUT: Duration = Duration::from_millis(300);

/// The repeat delay and interval set in Windows' keyboard settings.
fn repeat_timing() -> (Duration, Duration) {
    let get = |action| {
        let mut v = 0u32;
        let ok = unsafe {
            SystemParametersInfoW(
                action,
                0,
                Some((&mut v as *mut u32).cast()),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
        };
        ok.is_ok().then_some(v)
    };
    // 0–3: 250 ms steps from 250 ms. 0–31: about 2.5 to 30 per second.
    let delay = get(SPI_GETKEYBOARDDELAY).unwrap_or(1).min(3);
    let speed = get(SPI_GETKEYBOARDSPEED).unwrap_or(31).min(31);
    let per_second = 2.5 + speed as f64 * 27.5 / 31.0;
    (
        Duration::from_millis(250 * (delay as u64 + 1)),
        Duration::from_secs_f64(1.0 / per_second),
    )
}

impl SendInputInjector {
    pub fn new() -> Self {
        crate::win::follow_input_desktop();
        Self {
            keys: HashSet::new(),
            buttons: HashSet::new(),
            repeat: None,
            held_at: None,
        }
    }

    fn send(&self, inputs: &[INPUT]) -> Result<()> {
        let sent = unsafe { SendInput(inputs, size_of::<INPUT>() as i32) };
        if sent as usize == inputs.len() {
            return Ok(());
        }
        // Blocked input usually means the input desktop changed (lock
        // screen, UAC). Follow it and retry once.
        if crate::win::follow_input_desktop().is_some() {
            let sent = unsafe { SendInput(inputs, size_of::<INPUT>() as i32) };
            if sent as usize == inputs.len() {
                return Ok(());
            }
        }
        bail!(
            "SendInput was blocked: {}",
            windows::core::Error::from_thread()
        )
    }

    /// Type text as Unicode key events; Enter, Tab and Backspace as keys.
    fn text(&mut self, text: &str) -> Result<()> {
        for c in text.chars() {
            let key = match c {
                '\n' | '\r' => Some(keys::ENTER),
                '\t' => Some(keys::TAB),
                '\u{8}' => Some(keys::BACKSPACE),
                _ => None,
            };
            if let Some(usage) = key {
                self.key(usage, true)?;
                self.key(usage, false)?;
                continue;
            }
            let mut units = [0u16; 2];
            for &unit in c.encode_utf16(&mut units).iter() {
                for flags in [KEYEVENTF_UNICODE, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP] {
                    self.send(&[INPUT {
                        r#type: INPUT_KEYBOARD,
                        Anonymous: INPUT_0 {
                            ki: KEYBDINPUT {
                                wVk: VIRTUAL_KEY(0),
                                wScan: unit,
                                dwFlags: flags,
                                ..Default::default()
                            },
                        },
                    }])?;
                }
            }
        }
        Ok(())
    }

    fn key(&mut self, usage: u16, down: bool) -> Result<()> {
        let mut flags = if down {
            KEYBD_EVENT_FLAGS(0)
        } else {
            KEYEVENTF_KEYUP
        };
        let ki = if usage == keys::PAUSE {
            KEYBDINPUT {
                wVk: VK_PAUSE,
                dwFlags: flags,
                ..Default::default()
            }
        } else {
            let Some(code) = keys::hid_to_set1(usage) else {
                return Ok(());
            };
            flags |= KEYEVENTF_SCANCODE;
            if code & keys::EXTENDED != 0 {
                flags |= KEYEVENTF_EXTENDEDKEY;
            }
            KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: code & 0xFF,
                dwFlags: flags,
                ..Default::default()
            }
        };
        if down {
            self.keys.insert(usage);
            if self.held_at.is_some() {
                self.held_at = Some(Instant::now());
            }
            // Modifiers (left/right Ctrl, Shift, Alt, GUI) do not repeat.
            if !(0xE0..=0xE7).contains(&usage) && self.repeat.is_none_or(|(u, ..)| u != usage) {
                let (delay, interval) = repeat_timing();
                self.repeat = Some((usage, Instant::now() + delay, interval));
            }
        } else {
            self.keys.remove(&usage);
            if self.repeat.is_some_and(|(u, ..)| u == usage) {
                self.repeat = None;
            }
        }
        self.send(&[INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 { ki },
        }])
    }

    fn mouse(&self, flags: MOUSE_EVENT_FLAGS, dx: i32, dy: i32, data: i32) -> Result<()> {
        self.send(&[INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    mouseData: data as u32,
                    dwFlags: flags,
                    ..Default::default()
                },
            },
        }])
    }

    fn button(&mut self, button: MouseButton, down: bool) -> Result<()> {
        let (flags, data) = match (button, down) {
            (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
            (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
            (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
            (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
            (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
            (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (MouseButton::Back, true) => (MOUSEEVENTF_XDOWN, XBUTTON1),
            (MouseButton::Back, false) => (MOUSEEVENTF_XUP, XBUTTON1),
            (MouseButton::Forward, true) => (MOUSEEVENTF_XDOWN, XBUTTON2),
            (MouseButton::Forward, false) => (MOUSEEVENTF_XUP, XBUTTON2),
        };
        if down {
            self.buttons.insert(button);
        } else {
            self.buttons.remove(&button);
        }
        self.mouse(flags, 0, 0, data)
    }
}

impl Injector for SendInputInjector {
    fn next_tick(&self) -> Option<Instant> {
        self.repeat.map(|(_, at, _)| at)
    }

    fn tick(&mut self) {
        let Some((usage, at, interval)) = self.repeat else {
            return;
        };
        if Instant::now() < at {
            return;
        }
        // Keep to the schedule, unless far behind it.
        let now = Instant::now();
        let next = if at + interval > now {
            at + interval
        } else {
            now + interval
        };
        self.repeat = Some((usage, next, interval));
        if self
            .held_at
            .is_some_and(|held| now.duration_since(held) > HELD_TIMEOUT)
        {
            return; // the client went quiet: wait for it
        }
        if let Err(e) = self.key(usage, true) {
            tracing::debug!("key repeat failed: {e:#}");
            self.repeat = None;
        }
    }

    fn inject(&mut self, event: InputEvent, geometry: Geometry) -> Result<()> {
        match event {
            InputEvent::MouseMove { x, y, .. } => {
                let (vx, vy, vw, vh) = unsafe {
                    (
                        GetSystemMetrics(SM_XVIRTUALSCREEN),
                        GetSystemMetrics(SM_YVIRTUALSCREEN),
                        GetSystemMetrics(SM_CXVIRTUALSCREEN).max(2),
                        GetSystemMetrics(SM_CYVIRTUALSCREEN).max(2),
                    )
                };
                let px = geometry.left + x - vx;
                let py = geometry.top + y - vy;
                // Normalised 0..=65535 across the virtual desktop.
                let nx = ((px as i64 * 65535 + (vw as i64 - 1) / 2) / (vw as i64 - 1)) as i32;
                let ny = ((py as i64 * 65535 + (vh as i64 - 1) / 2) / (vh as i64 - 1)) as i32;
                self.mouse(
                    MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                    nx,
                    ny,
                    0,
                )
            }
            InputEvent::MouseMotion { dx, dy } => self.mouse(MOUSEEVENTF_MOVE, dx, dy, 0),
            InputEvent::MouseButton { button, down } => self.button(button, down),
            InputEvent::MouseWheel { dx, dy } => {
                if dy != 0 {
                    self.mouse(MOUSEEVENTF_WHEEL, 0, 0, dy)?;
                }
                if dx != 0 {
                    self.mouse(MOUSEEVENTF_HWHEEL, 0, 0, dx)?;
                }
                Ok(())
            }
            InputEvent::Key { usage, down } => self.key(usage, down),
            InputEvent::ReleaseAll => {
                for usage in self.keys.clone() {
                    let _ = self.key(usage, false);
                }
                for button in self.buttons.clone() {
                    let _ = self.button(button, false);
                }
                Ok(())
            }
            InputEvent::Text { text } => self.text(&text),
            InputEvent::KeysHeld { usages } => {
                self.held_at = Some(Instant::now());
                // A release lost on the way (or never sent) ends the repeat.
                if self.repeat.is_some_and(|(u, ..)| !usages.contains(&u)) {
                    self.repeat = None;
                }
                Ok(())
            }
            InputEvent::SecureAttention => {
                // Only a service may send it; ask ours, or try directly.
                if !crate::service::request_secure_attention() {
                    crate::win::send_secure_attention();
                }
                Ok(())
            }
        }
    }
}
