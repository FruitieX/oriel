//! Input injection.

use anyhow::Result;
use oriel_proto::InputEvent;

use crate::capture::{Geometry, SourceKind, TestState};

#[cfg(windows)]
mod windows;

pub trait Injector: Send {
    fn inject(&mut self, event: InputEvent, geometry: Geometry) -> Result<()>;
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
        _ => Ok(Box::new(TestInjector(test.clone()))),
    }
}
