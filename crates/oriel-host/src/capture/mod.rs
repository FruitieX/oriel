//! Screen capture backends.

use std::{sync::Arc, time::Duration};

use anyhow::Result;
use oriel_media::HwDevice;
use oriel_proto::{CursorShape, DisplayInfo};
use parking_lot::Mutex;

use crate::cursor::CursorPos;

#[cfg(target_os = "android")]
mod android;
#[cfg(windows)]
mod dxgi;
#[cfg(target_os = "linux")]
mod pipewire;
#[cfg(windows)]
mod sudovda;
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

    /// Something to keep alive while capturing the same target again
    /// (a virtual monitor), so a restarted capture finds it unchanged.
    fn keep_alive(&self) -> Option<Arc<dyn std::any::Any + Send + Sync>> {
        None
    }

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

/// What to capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// A physical display (`None`: the primary one).
    Display(Option<u32>),
    /// A virtual monitor created for the client.
    Virtual(oriel_proto::VirtualMode),
}

/// Whether this host can create virtual monitors.
pub fn virtual_display_available(source: SourceKind) -> bool {
    match source {
        #[cfg(target_os = "linux")]
        SourceKind::Auto => crate::gnome::GnomeSession::available(),
        #[cfg(windows)]
        SourceKind::Auto => sudovda::available(),
        _ => false,
    }
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
        #[cfg(target_os = "android")]
        SourceKind::Auto => android::display().into_iter().collect(),
        _ => vec![test_pattern::display()],
    }
}

/// A display mode set for the session, restored when dropped.
pub trait DisplayMode: Send {
    /// The size actually set (the closest the display supports).
    fn size(&self) -> (u32, u32);
    /// Set the mode again if Windows dropped it (a monitor that slept or
    /// reconnected comes back in its own mode). True when it did.
    fn reapply(&self) -> bool;
}

pub type ModeGuard = Box<dyn DisplayMode>;

/// Of the sizes a display supports, the one closest to `want`: the same
/// shape first, then the nearest size.
pub fn closest_mode(modes: &[(u32, u32)], want: (u32, u32)) -> Option<(u32, u32)> {
    let ln = |a: f64, b: f64| (a / b).ln().abs();
    let cost = |&(w, h): &(u32, u32)| {
        let shape = ln(w as f64 / h as f64, want.0 as f64 / want.1 as f64);
        let area = ln((w * h) as f64, (want.0 * want.1) as f64);
        // A 2% different shape still counts as the same.
        let shape = if shape < 0.02 { 0.0 } else { 10.0 + shape };
        shape + area
    };
    modes
        .iter()
        .filter(|(w, h)| *w > 0 && *h > 0)
        .min_by(|a, b| cost(a).total_cmp(&cost(b)))
        .copied()
}

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

pub fn open(source: SourceKind, target: Target, test: &TestState) -> Result<Box<dyn Capturer>> {
    match source {
        #[cfg(target_os = "android")]
        SourceKind::Auto => {
            let _ = target;
            Ok(Box::new(android::ScreenShare::open()?))
        }
        #[cfg(windows)]
        SourceKind::Auto => match target {
            Target::Display(display) => Ok(Box::new(dxgi::DxgiCapturer::open(display)?)),
            Target::Virtual(mode) => Ok(Box::new(dxgi::DxgiCapturer::open_virtual(
                sudovda::acquire(mode)?,
            )?)),
        },
        #[cfg(target_os = "linux")]
        SourceKind::Auto => {
            if !crate::gnome::GnomeSession::available() {
                anyhow::bail!("hosting on Linux needs a GNOME session for now");
            }
            Ok(Box::new(pipewire::PipeWireCapturer::open(target)?))
        }
        _ => {
            let _ = target;
            Ok(Box::new(test_pattern::TestPattern::new(test.clone())))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::closest_mode;

    #[test]
    fn closest_mode_keeps_the_shape() {
        let modes = [
            (3840, 2160),
            (2560, 1440),
            (1920, 1200),
            (1920, 1080),
            (1680, 1050),
            (1280, 1024),
            (1280, 720),
            (800, 600),
        ];
        assert_eq!(closest_mode(&modes, (1920, 1080)), Some((1920, 1080)));
        // Half of a 2560×1440 screen: 16:9 again.
        assert_eq!(closest_mode(&modes, (1280, 720)), Some((1280, 720)));
        // 16:10 at a size the display lacks: the nearest 16:10 one.
        assert_eq!(closest_mode(&modes, (1440, 900)), Some((1680, 1050)));
        // Tiny: the smallest of that shape.
        assert_eq!(closest_mode(&modes, (256, 144)), Some((1280, 720)));
        // No mode of that shape: the nearest size.
        assert_eq!(closest_mode(&modes, (1000, 1000)), Some((1280, 1024)));
        assert_eq!(closest_mode(&[], (1000, 1000)), None);
    }
}
