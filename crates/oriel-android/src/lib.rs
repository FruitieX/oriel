//! The Android app: a NativeActivity whose `android_main` runs the client.

#[cfg(target_os = "android")]
mod app;
#[cfg(target_os = "android")]
mod share;
