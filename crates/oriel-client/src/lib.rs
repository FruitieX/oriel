//! The oriel client: connects to a host, decodes its video and draws it,
//! and forwards input.

#[cfg(target_os = "android")]
pub mod android;
pub mod app;
pub mod autostart;
pub mod config;
pub mod connection;
mod decode;
mod diagnostics;
mod playout;

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
#[cfg(zero_copy)]
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

/// What this build is, for Settings → App.
pub fn build_info() -> [(&'static str, String); 4] {
    [
        ("Version", env!("CARGO_PKG_VERSION").to_string()),
        ("Commit", env!("ORIEL_GIT_SHA").to_string()),
        ("Built", env!("ORIEL_BUILD_DATE").to_string()),
        (
            "Platform",
            format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
        ),
    ]
}

/// Running in the Android emulator (whose GPU emulation has gaps).
pub fn android_emulator() -> bool {
    #[cfg(target_os = "android")]
    return android_system_properties::AndroidSystemProperties::new()
        .get("ro.hardware")
        .is_some_and(|h| h == "ranchu" || h == "goldfish");
    #[cfg(not(target_os = "android"))]
    false
}
