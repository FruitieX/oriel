use winit::platform::android::activity::AndroidApp;

#[unsafe(no_mangle)]
fn android_main(app: AndroidApp) {
    init_logging();
    oriel_client::android::init(app);
    if let Err(e) = run() {
        tracing::error!("oriel failed: {e:#}");
    }
}

fn run() -> anyhow::Result<()> {
    let paths = oriel_net::Paths {
        config_dir: oriel_client::android::data_dir()?,
    };
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let host: Box<dyn oriel_client::launcher::HostControl> =
        Box::new(crate::share::PhoneHost::new(paths.clone())?);
    oriel_client::launcher::run(
        rt,
        paths,
        Some(host),
        oriel_client::launcher::GuiOptions::default(),
    )
}

/// tracing → logcat (tag "oriel"); `ORIEL_LOG` does not reach an app, so
/// the level is fixed: info, debug for oriel itself in debug builds.
fn init_logging() {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};
    let filter = if cfg!(debug_assertions) {
        "info,oriel_client=debug,oriel_media=debug,oriel_android=debug"
    } else {
        "info"
    };
    let _ = tracing_subscriber::registry()
        .with(EnvFilter::new(filter))
        .with(
            fmt::layer()
                .with_ansi(false)
                .without_time()
                .with_writer(|| Logcat(Vec::new())),
        )
        .try_init();
    std::panic::set_hook(Box::new(|info| {
        tracing::error!("panic: {info}");
    }));
}

/// One log record, written to logcat when dropped.
struct Logcat(Vec<u8>);

impl std::io::Write for Logcat {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for Logcat {
    fn drop(&mut self) {
        while self.0.last().is_some_and(|b| *b == b'\n') {
            self.0.pop();
        }
        if self.0.is_empty() {
            return;
        }
        let text = std::ffi::CString::new(
            std::mem::take(&mut self.0)
                .into_iter()
                .filter(|b| *b != 0)
                .collect::<Vec<u8>>(),
        )
        .unwrap_or_default();
        // Level from the record's text: fmt puts it first.
        let s = text.to_bytes();
        let prio = if s.starts_with(b"ERROR") || s.starts_with(b" ERROR") {
            ndk_sys::android_LogPriority::ANDROID_LOG_ERROR
        } else if s.starts_with(b" WARN") || s.starts_with(b"WARN") {
            ndk_sys::android_LogPriority::ANDROID_LOG_WARN
        } else if s.starts_with(b"DEBUG") || s.starts_with(b"TRACE") {
            ndk_sys::android_LogPriority::ANDROID_LOG_DEBUG
        } else {
            ndk_sys::android_LogPriority::ANDROID_LOG_INFO
        };
        unsafe {
            ndk_sys::__android_log_write(prio.0 as i32, c"oriel".as_ptr(), text.as_ptr());
        }
    }
}

// Called by OrielActivity (android/java/.../OrielActivity.java).

use jni::{
    JNIEnv,
    objects::{JClass, JString},
    sys::{jboolean, jint},
};
use oriel_client::android::{Insets, TextInput, push_input};

#[unsafe(no_mangle)]
extern "system" fn Java_org_fruitiex_oriel_OrielActivity_nativeText(
    mut env: JNIEnv,
    _class: JClass,
    text: JString,
) {
    if let Ok(text) = env.get_string(&text) {
        push_input(TextInput::Text(text.into()));
    }
}

#[unsafe(no_mangle)]
extern "system" fn Java_org_fruitiex_oriel_OrielActivity_nativeDelete(
    _env: JNIEnv,
    _class: JClass,
    before: jint,
    after: jint,
) {
    push_input(TextInput::Delete {
        before: before.max(0) as u32,
        after: after.max(0) as u32,
    });
}

#[unsafe(no_mangle)]
extern "system" fn Java_org_fruitiex_oriel_OrielActivity_nativeKey(
    _env: JNIEnv,
    _class: JClass,
    code: jint,
    down: jboolean,
    meta: jint,
) {
    push_input(TextInput::Key {
        code,
        down: down != 0,
        meta,
    });
}

#[unsafe(no_mangle)]
extern "system" fn Java_org_fruitiex_oriel_OrielActivity_nativeKeyboardShown(
    _env: JNIEnv,
    _class: JClass,
    shown: jboolean,
) {
    oriel_client::android::set_keyboard_shown(shown != 0);
}

#[unsafe(no_mangle)]
extern "system" fn Java_org_fruitiex_oriel_OrielActivity_nativeInsets(
    _env: JNIEnv,
    _class: JClass,
    left: jint,
    top: jint,
    right: jint,
    bottom: jint,
    keyboard: jint,
) {
    oriel_client::android::set_insets(Insets {
        bars: [left as f32, top as f32, right as f32, bottom as f32],
        keyboard: keyboard as f32,
    });
}

#[unsafe(no_mangle)]
extern "system" fn Java_org_fruitiex_oriel_OrielActivity_nativeShareStarted(
    _env: JNIEnv,
    _class: JClass,
    width: jint,
    height: jint,
) {
    tracing::info!(width, height, "screen share started");
    oriel_media::share::set_screen_size(Some((width as u32, height as u32)));
}

#[unsafe(no_mangle)]
extern "system" fn Java_org_fruitiex_oriel_OrielActivity_nativeShareStopped(
    _env: JNIEnv,
    _class: JClass,
) {
    tracing::info!("screen share stopped");
    oriel_media::share::set_screen_size(None);
}

#[unsafe(no_mangle)]
extern "system" fn Java_org_fruitiex_oriel_OrielActivity_nativeEncodedFrame(
    env: JNIEnv,
    _class: JClass,
    data: jni::objects::JByteArray,
    keyframe: jboolean,
    encode_us: jint,
) {
    if let Ok(bytes) = env.convert_byte_array(&data) {
        oriel_media::share::push(bytes, keyframe != 0, encode_us.max(0) as u32);
    }
}

#[unsafe(no_mangle)]
extern "system" fn Java_org_fruitiex_oriel_OrielActivity_nativePushToken(
    mut env: JNIEnv,
    _class: JClass,
    token: JString,
) {
    let Ok(token) = env.get_string(&token).map(String::from) else {
        return;
    };
    if let Ok(dir) = oriel_client::android::data_dir() {
        crate::share::push_token(oriel_net::Paths { config_dir: dir }, token);
    }
}
