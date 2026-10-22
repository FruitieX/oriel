//! The oriel client: connects to a host, decodes its video and draws it,
//! and forwards input.

pub mod app;
pub mod config;
pub mod connection;
mod decode;
pub mod headless;
#[cfg(target_os = "linux")]
mod inhibit;
pub mod keymap;
pub mod launcher;
mod render;
pub mod session;
mod stats;

pub use app::{MouseMode, ViewerOptions};
