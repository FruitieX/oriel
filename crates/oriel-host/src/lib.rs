//! The oriel host: captures a display, encodes it and streams it to a paired
//! client, and injects the client's input.

pub mod capture;
mod clipboard;
mod cursor;
pub mod input;
mod pipeline;
mod server;
mod session;
#[cfg(windows)]
pub mod win;

pub use capture::SourceKind;
pub use server::{HostOptions, run};

/// Per-process setup that must happen before any capture or input call.
pub fn init_process() {
    #[cfg(windows)]
    win::init_process();
}
