//! Android glue: the activity handle, set once by the app's `android_main`.

use std::{path::PathBuf, sync::OnceLock};

use anyhow::{Context, Result};
use winit::platform::android::activity::AndroidApp;

static APP: OnceLock<AndroidApp> = OnceLock::new();

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
