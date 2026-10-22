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
