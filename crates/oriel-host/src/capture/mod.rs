//! Screen capture backends.

use std::{sync::Arc, time::Duration};

use anyhow::Result;
use oriel_media::HwDevice;
use oriel_proto::{CursorShape, DisplayInfo};
use parking_lot::Mutex;

use crate::cursor::CursorPos;

#[cfg(windows)]
mod dxgi;
#[cfg(target_os = "linux")]
mod pipewire;
pub mod test_pattern;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SourceKind {
    /// The platform's real capture backend.
    #[default]
    Auto,
    /// Synthetic animated frames; input is drawn instead of injected.
    TestPattern,
}

/// Where the captured surface sits on the host's desktop, for mapping
/// absolute mouse positions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Geometry {
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
}

pub trait Capturer {
    fn size(&self) -> (u32, u32);

    fn geometry(&self) -> Geometry;

    /// Device the frames live on, for zero-copy encoding.
    fn hw_device(&self) -> HwDevice {
        HwDevice::None
    }

    /// Wait up to `timeout` for the screen to change. The latest image is
    /// kept, so it can be encoded later (rate limiting, keyframe requests).
    /// Returns whether a new image arrived. Errors mean the capture must be
    /// recreated (display mode change, desktop switch, ...).
    fn next_frame(&mut self, timeout: Duration) -> Result<bool>;

    /// The latest image as BGRA in CPU memory.
    fn cpu_frame(&mut self) -> Result<(&[u8], usize)>;

    /// Copy the latest image into a BGRA `ID3D11Texture2D` array slice.
    #[cfg(windows)]
    fn copy_to_texture(&mut self, _dst: *mut std::ffi::c_void, _index: u32) -> Result<()> {
        anyhow::bail!("capture source has no GPU frames")
    }

    fn cursor(&mut self) -> CursorPos;

    /// A new cursor image, if it changed since the last call.
    fn cursor_shape(&mut self) -> Option<CursorShape>;
}

/// State shared between the test pattern source and its input sink.
pub type TestState = Arc<Mutex<test_pattern::State>>;

pub fn list_displays(source: SourceKind) -> Vec<DisplayInfo> {
    match source {
        #[cfg(windows)]
        SourceKind::Auto => dxgi::list_displays().unwrap_or_else(|e| {
            tracing::warn!("cannot enumerate displays: {e:#}");
            Vec::new()
        }),
        #[cfg(target_os = "linux")]
        SourceKind::Auto => crate::gnome::list_displays().unwrap_or_else(|e| {
            tracing::warn!("cannot enumerate displays: {e:#}");
            Vec::new()
        }),
        _ => vec![test_pattern::display()],
    }
}

/// Something that restores the display mode when dropped.
pub type ModeGuard = Box<dyn std::any::Any + Send>;

/// Switch the host display's resolution for the session.
pub fn set_display_mode(
    source: SourceKind,
    display: Option<u32>,
    (width, height): (u32, u32),
) -> Result<ModeGuard> {
    match source {
        #[cfg(windows)]
        SourceKind::Auto => Ok(Box::new(dxgi::set_display_mode(display, width, height)?)),
        _ => {
            let _ = (display, width, height);
            anyhow::bail!("this host cannot change its resolution")
        }
    }
}

pub fn open(
    source: SourceKind,
    display: Option<u32>,
    test: &TestState,
) -> Result<Box<dyn Capturer>> {
    match source {
        #[cfg(windows)]
        SourceKind::Auto => Ok(Box::new(dxgi::DxgiCapturer::open(display)?)),
        #[cfg(target_os = "linux")]
        SourceKind::Auto => {
            if !crate::gnome::GnomeSession::available() {
                anyhow::bail!("hosting on Linux needs a GNOME session for now");
            }
            Ok(Box::new(pipewire::PipeWireCapturer::open(display)?))
        }
        _ => {
            let _ = display;
            Ok(Box::new(test_pattern::TestPattern::new(test.clone())))
        }
    }
}
