//! Windows process and thread setup.

use windows::Win32::{
    Foundation::GENERIC_ALL,
    Media::timeBeginPeriod,
    System::{
        StationsAndDesktops::{
            CloseDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, GetThreadDesktop,
            GetUserObjectInformationW, HDESK, OpenInputDesktop, SetThreadDesktop, UOI_NAME,
        },
        Threading::{
            ABOVE_NORMAL_PRIORITY_CLASS, GetCurrentProcess, GetCurrentThread, GetCurrentThreadId,
            SetPriorityClass, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
        },
    },
    UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext},
};

pub fn init_process() {
    watch_display_power();
    unsafe {
        // Physical pixel coordinates everywhere (capture, cursor, SendInput).
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        // 1 ms timer resolution for frame pacing sleeps.
        timeBeginPeriod(1);
        let _ = SetPriorityClass(GetCurrentProcess(), ABOVE_NORMAL_PRIORITY_CLASS);
    }
}

pub fn boost_thread_priority() {
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
    }
}

fn desktop_name(desktop: HDESK) -> String {
    let mut buf = [0u16; 128];
    let mut needed = 0u32;
    unsafe {
        if GetUserObjectInformationW(
            windows::Win32::Foundation::HANDLE(desktop.0),
            UOI_NAME,
            Some(buf.as_mut_ptr().cast()),
            (buf.len() * 2) as u32,
            Some(&mut needed),
        )
        .is_err()
        {
            return String::new();
        }
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// Attach the calling thread to the desktop that currently receives input
/// (Default, Winlogon for the lock screen and UAC prompts, ...). Returns the
/// desktop's name when the thread switched, `None` when nothing changed or
/// switching is not possible (only SYSTEM can open the Winlogon desktop).
pub fn follow_input_desktop() -> Option<String> {
    unsafe {
        let input = OpenInputDesktop(
            DESKTOP_CONTROL_FLAGS(0),
            false,
            DESKTOP_ACCESS_FLAGS(GENERIC_ALL.0),
        )
        .ok()?;
        let input_name = desktop_name(input);
        let current = GetThreadDesktop(GetCurrentThreadId()).ok();
        let current_name = current.map(desktop_name).unwrap_or_default();
        if input_name == current_name {
            let _ = CloseDesktop(input);
            return None;
        }
        if SetThreadDesktop(input).is_err() {
            let _ = CloseDesktop(input);
            return None;
        }
        tracing::info!(from = %current_name, to = %input_name, "switched to input desktop");
        Some(input_name)
    }
}

/// Ctrl+Alt+Del. Only works from a service running as SYSTEM (and with the
/// "software SAS" policy allowing services); otherwise Windows ignores it.
pub fn send_secure_attention() {
    use windows::{
        Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW},
        core::{s, w},
    };
    unsafe {
        let Ok(lib) = LoadLibraryW(w!("sas.dll")) else {
            tracing::warn!("sas.dll not available; cannot send Ctrl+Alt+Del");
            return;
        };
        let Some(proc) = GetProcAddress(lib, s!("SendSAS")) else {
            tracing::warn!("SendSAS not found");
            return;
        };
        let send_sas: unsafe extern "system" fn(i32) = std::mem::transmute(proc);
        send_sas(0);
        tracing::info!("sent Ctrl+Alt+Del");
    }
}

/// GUID_CONSOLE_DISPLAY_STATE: 0 off, 1 on, 2 dimmed; unknown until
/// Windows reports it, which it does right after registration.
static DISPLAY_STATE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(u32::MAX);

/// Follow whether Windows has switched the displays off (power saving).
fn watch_display_power() {
    use windows::Win32::{
        Foundation::{ERROR_SUCCESS, HANDLE},
        System::{
            Power::{DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS, PowerSettingRegisterNotification},
            SystemServices::GUID_CONSOLE_DISPLAY_STATE,
        },
        UI::WindowsAndMessaging::DEVICE_NOTIFY_CALLBACK,
    };
    // Registered for the life of the process.
    let params: &'static mut _ = Box::leak(Box::new(DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS {
        Callback: Some(on_display_state),
        Context: std::ptr::null_mut(),
    }));
    let mut handle = std::ptr::null_mut();
    let err = unsafe {
        PowerSettingRegisterNotification(
            &GUID_CONSOLE_DISPLAY_STATE,
            DEVICE_NOTIFY_CALLBACK,
            HANDLE(params as *mut DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS as *mut _),
            &mut handle,
        )
    };
    if err != ERROR_SUCCESS {
        tracing::warn!("cannot follow the display power state: {err:?}");
    }
}

unsafe extern "system" fn on_display_state(
    _context: *const std::ffi::c_void,
    kind: u32,
    setting: *const std::ffi::c_void,
) -> u32 {
    use windows::Win32::{
        System::{Power::POWERBROADCAST_SETTING, SystemServices::GUID_CONSOLE_DISPLAY_STATE},
        UI::WindowsAndMessaging::PBT_POWERSETTINGCHANGE,
    };
    if kind != PBT_POWERSETTINGCHANGE || setting.is_null() {
        return 0;
    }
    let setting = setting as *const POWERBROADCAST_SETTING;
    unsafe {
        if (*setting).PowerSetting == GUID_CONSOLE_DISPLAY_STATE && (*setting).DataLength >= 4 {
            let state = std::ptr::read_unaligned((*setting).Data.as_ptr() as *const u32);
            DISPLAY_STATE.store(state, std::sync::atomic::Ordering::Relaxed);
            tracing::debug!(state, "display power state");
        }
    }
    0
}

/// Windows has switched the displays off: none would show a picture.
pub fn displays_off() -> bool {
    DISPLAY_STATE.load(std::sync::atomic::Ordering::Relaxed) == 0
}
