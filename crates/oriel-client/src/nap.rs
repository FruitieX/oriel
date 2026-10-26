//! While a session runs, macOS must not put oriel in App Nap: a hidden
//! window's app is throttled (timers, I/O, thread priority), the stream
//! falls behind and takes seconds to catch up when the window returns.

#[cfg(target_os = "macos")]
pub struct Activity(
    objc2::rc::Retained<objc2::runtime::ProtocolObject<dyn objc2_foundation::NSObjectProtocol>>,
);

#[cfg(not(target_os = "macos"))]
pub struct Activity;

/// Hold the returned value for as long as the work lasts.
pub fn begin(reason: &str) -> Activity {
    #[cfg(target_os = "macos")]
    {
        use objc2_foundation::{NSActivityOptions, NSProcessInfo, NSString};
        // Latency-critical user work, which still lets the Mac sleep when
        // idle.
        let options = NSActivityOptions::UserInitiatedAllowingIdleSystemSleep
            | NSActivityOptions::LatencyCritical;
        let token = NSProcessInfo::processInfo()
            .beginActivityWithOptions_reason(options, &NSString::from_str(reason));
        Activity(token)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = reason;
        Activity
    }
}

#[cfg(target_os = "macos")]
impl Drop for Activity {
    fn drop(&mut self) {
        unsafe { objc2_foundation::NSProcessInfo::processInfo().endActivity(&self.0) };
    }
}
