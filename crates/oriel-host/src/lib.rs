//! The oriel host: captures a display, encodes it and streams it to a paired
//! client, and injects the client's input.

pub mod capture;
mod clipboard;
mod cursor;
#[cfg(target_os = "linux")]
mod gnome;
pub mod input;
mod pipeline;
mod server;
#[cfg(windows)]
pub mod service;
mod session;
#[cfg(windows)]
pub mod win;

pub use capture::SourceKind;
pub use server::{ClientInfo, HostControl, HostOptions, HostStatus, run, run_with};

/// Microseconds on the host clock that frame timestamps and pongs use.
pub(crate) fn host_us() -> u64 {
    static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    EPOCH
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_micros() as u64
}

/// Whether this platform can share its real desktop (Windows, or Linux
/// under GNOME); otherwise only the test pattern is available.
pub fn native_hosting_available() -> bool {
    #[cfg(windows)]
    return true;
    #[cfg(target_os = "linux")]
    return gnome::GnomeSession::available();
    #[allow(unreachable_code)]
    false
}

/// Per-process setup that must happen before any capture or input call.
pub fn init_process() {
    #[cfg(windows)]
    win::init_process();
}
