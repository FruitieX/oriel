//! Input injection.

use anyhow::Result;
use oriel_proto::InputEvent;

use crate::capture::{Geometry, SourceKind, TestState};

#[cfg(target_os = "android")]
pub mod android;
#[cfg(target_os = "linux")]
mod gnome;
#[cfg(windows)]
mod windows;

pub trait Injector {
    fn inject(&mut self, event: InputEvent, geometry: Geometry) -> Result<()>;

    /// When [`Injector::tick`] should next run: injectors whose platform
    /// does not repeat held keys repeat them themselves.
    fn next_tick(&self) -> Option<std::time::Instant> {
        None
    }

    fn tick(&mut self) {}
}

/// Feeds input to the test pattern, which draws it.
struct TestInjector(TestState);

impl Injector for TestInjector {
    fn inject(&mut self, event: InputEvent, _geometry: Geometry) -> Result<()> {
        self.0.lock().apply(event);
        Ok(())
    }
}

pub fn open(source: SourceKind, test: &TestState) -> Result<Box<dyn Injector>> {
    match source {
        #[cfg(windows)]
        SourceKind::Auto => Ok(Box::new(windows::SendInputInjector::new())),
        #[cfg(target_os = "linux")]
        SourceKind::Auto => Ok(Box::new(gnome::GnomeInjector::new()?)),
        #[cfg(target_os = "android")]
        SourceKind::Auto => Ok(Box::new(android::AccessibilityInjector::default())),
        _ => Ok(Box::new(TestInjector(test.clone()))),
    }
}
