//! Android: the screen as the app's screen share encodes it (see
//! `oriel_media::share`). Capture and encoding happen together on the Java
//! side, so a "frame" here is an encoded one waiting for the pipeline.

use std::time::Duration;

use anyhow::{Result, bail};
use oriel_media::share;
use oriel_proto::{CursorShape, DisplayInfo};

use super::{Capturer, Geometry};
use crate::cursor::CursorPos;

pub struct ScreenShare {
    size: (u32, u32),
}

impl ScreenShare {
    pub fn open() -> Result<Self> {
        let Some(size) = share::screen_size() else {
            bail!("start sharing on the phone first");
        };
        Ok(Self { size })
    }
}

pub fn display() -> Option<DisplayInfo> {
    let (width, height) = share::screen_size()?;
    Some(DisplayInfo {
        id: 0,
        name: "Phone screen".into(),
        width,
        height,
        refresh_hz: 60,
        primary: true,
        modes: Vec::new(),
    })
}

impl Capturer for ScreenShare {
    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn geometry(&self) -> Geometry {
        Geometry {
            left: 0,
            top: 0,
            width: self.size.0,
            height: self.size.1,
        }
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<bool> {
        if share::screen_size() != Some(self.size) {
            bail!("the screen share stopped or turned");
        }
        Ok(share::wait(timeout))
    }

    fn cpu_frame(&mut self) -> Result<(&[u8], usize)> {
        bail!("the phone's screen share is encoded already")
    }

    fn cursor(&mut self) -> CursorPos {
        CursorPos::default()
    }

    fn cursor_shape(&mut self) -> Option<CursorShape> {
        None
    }
}
