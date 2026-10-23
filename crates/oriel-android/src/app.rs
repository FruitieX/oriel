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
    oriel_client::launcher::run(
        rt,
        paths,
        None,
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
