//! Windows: a low-level keyboard hook that takes every key while the remote
//! desktop has the keyboard, so Win, Alt+Tab, Ctrl+Esc and friends reach the
//! host instead of the local shell. (Ctrl+Alt+Del cannot be intercepted;
//! the menu sends it.)

use std::{
    collections::HashMap,
    sync::{
        OnceLock,
        atomic::{AtomicBool, AtomicIsize, Ordering},
    },
};

use parking_lot::Mutex;
use windows::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        Input::KeyboardAndMouse::VK_PAUSE,
        WindowsAndMessaging::{
            CallNextHookEx, GetForegroundWindow, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, LLKHF_EXTENDED,
            LLKHF_UP, SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL,
        },
    },
};

/// Keys taken by the hook: (HID usage, pressed).
static QUEUE: Mutex<Vec<(u16, bool)>> = Mutex::new(Vec::new());
static ACTIVE: AtomicBool = AtomicBool::new(false);
static WINDOW: AtomicIsize = AtomicIsize::new(0);
static WAKE: OnceLock<Mutex<Option<Box<dyn Fn() + Send>>>> = OnceLock::new();

fn set1_to_hid() -> &'static HashMap<u16, u16> {
    static TABLE: OnceLock<HashMap<u16, u16>> = OnceLock::new();
    TABLE.get_or_init(|| {
        (0u16..=0xE7)
            .filter_map(|usage| oriel_proto::keys::hid_to_set1(usage).map(|code| (code, usage)))
            .collect()
    })
}

unsafe extern "system" fn hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && ACTIVE.load(Ordering::Relaxed) {
        let foreground = unsafe { GetForegroundWindow() };
        if foreground.0 as isize == WINDOW.load(Ordering::Relaxed) {
            let kb = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
            let down = kb.flags.0 & LLKHF_UP.0 == 0;
            let usage = if kb.vkCode == VK_PAUSE.0 as u32 {
                Some(oriel_proto::keys::PAUSE)
            } else {
                let ext = if kb.flags.0 & LLKHF_EXTENDED.0 != 0 {
                    oriel_proto::keys::EXTENDED
                } else {
                    0
                };
                set1_to_hid().get(&(ext | kb.scanCode as u16)).copied()
            };
            if let Some(usage) = usage {
                QUEUE.lock().push((usage, down));
                if let Some(wake) = WAKE.get_or_init(Default::default).lock().as_ref() {
                    wake();
                }
                // Swallow it: the app forwards it to the host.
                return LRESULT(1);
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// Installed while the viewer window exists.
pub struct KeyboardHook(HHOOK);

impl KeyboardHook {
    /// `wake` is called (from the hook) when keys are queued.
    pub fn install(window: HWND, wake: impl Fn() + Send + 'static) -> Option<Self> {
        WINDOW.store(window.0 as isize, Ordering::Relaxed);
        *WAKE.get_or_init(Default::default).lock() = Some(Box::new(wake));
        let module = unsafe { GetModuleHandleW(None) }.ok()?;
        match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook), Some(module.into()), 0) } {
            Ok(h) => Some(Self(h)),
            Err(e) => {
                tracing::warn!("cannot install the keyboard hook: {e}");
                None
            }
        }
    }

    /// Take keys only while they should go to the host.
    pub fn set_active(&self, active: bool) {
        ACTIVE.store(active, Ordering::Relaxed);
        if !active {
            QUEUE.lock().clear();
        }
    }

    pub fn drain(&self) -> Vec<(u16, bool)> {
        std::mem::take(&mut *QUEUE.lock())
    }
}

impl Drop for KeyboardHook {
    fn drop(&mut self) {
        ACTIVE.store(false, Ordering::Relaxed);
        *WAKE.get_or_init(Default::default).lock() = None;
        unsafe {
            let _ = UnhookWindowsHookEx(self.0);
        }
    }
}
