//! Android glue: the activity handle (set once by the app's
//! `android_main`), and what the Java side of the activity
//! (`android/java/.../OrielActivity.java`) provides: soft keyboard text,
//! window insets and the clipboard.

use std::{
    path::PathBuf,
    sync::{
        OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Context, Result};
use jni::{
    JNIEnv, JavaVM,
    objects::{JObject, JString, JValue},
};
use parking_lot::Mutex;
use winit::{event_loop::EventLoopProxy, platform::android::activity::AndroidApp};

use crate::session::UiEvent;

static APP: OnceLock<AndroidApp> = OnceLock::new();
static PROXY: Mutex<Option<EventLoopProxy<UiEvent>>> = Mutex::new(None);
static INPUT: Mutex<Vec<TextInput>> = Mutex::new(Vec::new());
static INSETS: Mutex<Insets> = Mutex::new(Insets {
    bars: [0.0; 4],
    keyboard: 0.0,
});
static KEYBOARD: AtomicBool = AtomicBool::new(false);

pub fn init(app: AndroidApp) {
    let _ = APP.set(app);
}

pub fn app() -> &'static AndroidApp {
    APP.get().expect("oriel_client::android::init not called")
}

/// Where the identity, pairings and settings live.
pub fn data_dir() -> Result<PathBuf> {
    app().internal_data_path().context("no app data directory")
}

pub(crate) fn set_proxy(proxy: EventLoopProxy<UiEvent>) {
    *PROXY.lock() = Some(proxy);
}

fn wake() {
    if let Some(p) = &*PROXY.lock() {
        let _ = p.send_event(UiEvent::Platform);
    }
}

/// What the soft keyboard did.
#[derive(Debug, Clone, PartialEq)]
pub enum TextInput {
    Text(String),
    /// Characters to delete before and after the cursor.
    Delete {
        before: u32,
        after: u32,
    },
    /// A key sent as a key (an Android `KeyEvent` key code).
    Key {
        code: i32,
        down: bool,
        meta: i32,
    },
}

pub fn push_input(input: TextInput) {
    INPUT.lock().push(input);
    wake();
}

pub(crate) fn take_input() -> Vec<TextInput> {
    std::mem::take(&mut *INPUT.lock())
}

/// Window areas covered by system bars, the camera cut-out and the
/// keyboard, in pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Insets {
    /// Left, top, right, bottom.
    pub bars: [f32; 4],
    /// Height of the soft keyboard (0 when hidden).
    pub keyboard: f32,
}

pub fn set_insets(insets: Insets) {
    tracing::debug!(?insets, "window insets");
    *INSETS.lock() = insets;
    wake();
}

pub(crate) fn insets() -> Insets {
    *INSETS.lock()
}

/// The safe area for egui, in points.
pub(crate) fn safe_area(pixels_per_point: f32) -> egui::SafeAreaInsets {
    let i = insets();
    let p = |v: f32| v / pixels_per_point.max(0.01);
    egui::SafeAreaInsets(egui::epaint::MarginF32 {
        left: p(i.bars[0]),
        top: p(i.bars[1]),
        right: p(i.bars[2]),
        bottom: p(i.bars[3].max(i.keyboard)),
    })
}

pub fn set_keyboard_shown(shown: bool) {
    KEYBOARD.store(shown, Ordering::Relaxed);
    wake();
}

pub(crate) fn keyboard_shown() -> bool {
    KEYBOARD.load(Ordering::Relaxed)
}

/// Run `f` with the JNI environment and the activity object.
fn with_activity<T>(f: impl FnOnce(&mut JNIEnv, &JObject) -> jni::errors::Result<T>) -> Option<T> {
    let app = app();
    let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) }.ok()?;
    let mut env = vm.attach_current_thread().ok()?;
    let activity = unsafe { JObject::from_raw(app.activity_as_ptr().cast()) };
    let result = f(&mut env, &activity);
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_describe();
        let _ = env.exception_clear();
    }
    match result {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!("JNI call failed: {e}");
            None
        }
    }
}

pub(crate) fn show_keyboard(show: bool) {
    with_activity(|env, activity| {
        env.call_method(
            activity,
            "showKeyboard",
            "(Z)V",
            &[JValue::Bool(show.into())],
        )
        .map(|_| ())
    });
}

/// Send the app to the background (Back on the start page).
pub(crate) fn move_to_back() {
    with_activity(|env, activity| {
        env.call_method(activity, "moveTaskToBack", "(Z)Z", &[JValue::Bool(1)])
            .map(|_| ())
    });
}

pub(crate) fn clipboard_text() -> Option<String> {
    with_activity(|env, activity| {
        let obj = env
            .call_method(activity, "clipboardText", "()Ljava/lang/String;", &[])?
            .l()?;
        if obj.is_null() {
            return Ok(None);
        }
        let s = JString::from(obj);
        Ok(Some(env.get_string(&s)?.into()))
    })
    .flatten()
}

pub(crate) fn set_clipboard_text(text: &str) {
    with_activity(|env, activity| {
        let s = env.new_string(text)?;
        env.call_method(
            activity,
            "setClipboardText",
            "(Ljava/lang/String;)V",
            &[JValue::Object(&s)],
        )
        .map(|_| ())
    });
}

/// The USB HID usage of an Android key code, for keys a soft keyboard
/// sends as keys.
pub(crate) fn hid_usage(code: i32) -> Option<u16> {
    Some(match code {
        29..=54 => 0x04 + (code - 29) as u16,    // A..Z
        8..=16 => 0x1E + (code - 8) as u16,      // 1..9
        7 => 0x27,                               // 0
        66 | 160 => 0x28,                        // Enter, numpad Enter
        111 => 0x29,                             // Escape
        67 => 0x2A,                              // Backspace (KEYCODE_DEL)
        61 => 0x2B,                              // Tab
        62 => 0x2C,                              // Space
        112 => 0x4C,                             // Delete (KEYCODE_FORWARD_DEL)
        122 => 0x4A,                             // Home
        92 => 0x4B,                              // Page up
        123 => 0x4D,                             // End
        93 => 0x4E,                              // Page down
        22 => 0x4F,                              // Right
        21 => 0x50,                              // Left
        20 => 0x51,                              // Down
        19 => 0x52,                              // Up
        131..=142 => 0x3A + (code - 131) as u16, // F1..F12
        _ => return None,
    })
}

/// While an egui text field has focus: give it the soft keyboard's input,
/// and show or hide the keyboard as focus comes and goes.
pub(crate) fn feed_egui(ctx: &egui::Context, raw: &mut egui::RawInput) {
    static WANTED: AtomicBool = AtomicBool::new(false);
    let wants = ctx.text_edit_focused();
    if wants != WANTED.swap(wants, Ordering::Relaxed) {
        show_keyboard(wants);
    }
    if !wants {
        return;
    }
    let tap = |raw: &mut egui::RawInput, key: egui::Key| {
        for pressed in [true, false] {
            raw.events.push(egui::Event::Key {
                key,
                physical_key: None,
                pressed,
                repeat: false,
                modifiers: Default::default(),
            });
        }
    };
    for input in take_input() {
        match input {
            TextInput::Text(text) => {
                for (i, line) in text.split('\n').enumerate() {
                    if i > 0 {
                        tap(raw, egui::Key::Enter);
                    }
                    if !line.is_empty() {
                        raw.events.push(egui::Event::Text(line.to_string()));
                    }
                }
            }
            TextInput::Delete { before, after } => {
                for _ in 0..before.min(256) {
                    tap(raw, egui::Key::Backspace);
                }
                for _ in 0..after.min(256) {
                    tap(raw, egui::Key::Delete);
                }
            }
            TextInput::Key { code, down, .. } => {
                let key = match code {
                    66 | 160 => egui::Key::Enter,
                    67 => egui::Key::Backspace,
                    112 => egui::Key::Delete,
                    61 => egui::Key::Tab,
                    111 => egui::Key::Escape,
                    21 => egui::Key::ArrowLeft,
                    22 => egui::Key::ArrowRight,
                    19 => egui::Key::ArrowUp,
                    20 => egui::Key::ArrowDown,
                    _ => continue,
                };
                raw.events.push(egui::Event::Key {
                    key,
                    physical_key: None,
                    pressed: down,
                    repeat: false,
                    modifiers: Default::default(),
                });
            }
        }
    }
}
