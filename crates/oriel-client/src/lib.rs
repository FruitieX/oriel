//! The oriel client: connects to a host, decodes its video and draws it,
//! and forwards input.

pub mod app;
pub mod connection;
mod decode;
pub mod headless;
pub mod keymap;
mod render;
pub mod session;
mod stats;

pub use app::{MouseMode, ViewerOptions};
