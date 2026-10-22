//! The oriel client: connects to a host, decodes its video and draws it,
//! and forwards input.

pub mod app;
pub mod autostart;
pub mod config;
pub mod connection;
mod decode;

/// Name the window after the app, so desktops match it to oriel.desktop
/// (the Wayland app id / X11 WM_CLASS) for its name and icon.
pub(crate) fn app_window(
    attrs: winit::window::WindowAttributes,
) -> winit::window::WindowAttributes {
    #[cfg(target_os = "linux")]
    let attrs =
        winit::platform::wayland::WindowAttributesExtWayland::with_name(attrs, "oriel", "oriel");
    attrs
}
mod gamepad;
pub mod headless;
#[cfg(target_os = "linux")]
mod inhibit;
#[cfg(not(target_os = "macos"))]
pub mod interop;
pub mod keymap;
pub mod launcher;
mod render;
pub mod session;
pub mod settings_ui;
mod stats;
pub mod theme;
#[cfg(windows)]
mod tray;
#[cfg(windows)]
mod winkeys;

pub use app::{MouseMode, ViewerOptions};
