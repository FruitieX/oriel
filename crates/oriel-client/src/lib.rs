//! The oriel client: connects to a host, decodes its video and draws it,
//! and forwards input.

pub mod app;
pub mod config;
pub mod connection;
mod decode;
pub mod headless;
#[cfg(target_os = "linux")]
mod inhibit;
#[cfg(not(target_os = "macos"))]
pub mod interop;
pub mod keymap;
pub mod launcher;
mod render;
pub mod session;
mod stats;
#[cfg(windows)]
mod tray;
#[cfg(windows)]
mod winkeys;

pub use app::{MouseMode, ViewerOptions};
