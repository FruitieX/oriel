//! Input injection with `SendInput`.

use std::collections::HashSet;

use anyhow::{Result, bail};
use oriel_proto::{InputEvent, MouseButton, keys};
use windows::Win32::UI::{
    Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT,
        KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSE_EVENT_FLAGS,
        MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
        MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
        MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
        MOUSEEVENTF_XUP, MOUSEINPUT, SendInput, VIRTUAL_KEY, VK_PAUSE,
    },
    WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    },
};

use super::Injector;
use crate::capture::Geometry;

const XBUTTON1: i32 = 1;
const XBUTTON2: i32 = 2;

pub struct SendInputInjector {
    keys: HashSet<u16>,
    buttons: HashSet<MouseButton>,
}

impl SendInputInjector {
    pub fn new() -> Self {
        crate::win::follow_input_desktop();
        Self {
            keys: HashSet::new(),
            buttons: HashSet::new(),
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
        } else {
            self.keys.remove(&usage);
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
