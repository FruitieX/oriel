//! Video encoding and decoding on top of FFmpeg (raw `ffmpeg-sys-next`).
//!
//! The encoder takes either CPU BGRA frames or, on Windows, D3D11 textures
//! (zero-copy into NVENC/AMF). The decoder prefers the platform's hardware
//! decoder and hands out CPU-side YUV frames. Android has no FFmpeg: it
//! decodes with MediaCodec (see `android`) and cannot encode.

#[cfg(not(target_os = "android"))]
pub use ffmpeg_sys_next as ff;

mod formats;
pub use formats::{ColorMatrix, PlaneFormat};

#[cfg(not(target_os = "android"))]
mod decoder;
#[cfg(not(target_os = "android"))]
mod encoder;
#[cfg(not(target_os = "android"))]
pub mod pyrowave;
#[cfg(not(target_os = "android"))]
mod util;

#[cfg(not(target_os = "android"))]
pub use decoder::{DecodedFrame, Decoder, Frame};
#[cfg(target_os = "linux")]
pub use decoder::{DmabufFrame, DmabufPlane};
#[cfg(not(target_os = "android"))]
pub use encoder::{
    EncodedFrame, Encoder, EncoderConfig, EncoderInput, HwDevice, available_codecs,
    available_encoders,
};
#[cfg(not(target_os = "android"))]
pub use pyrowave::{
    EXTERNAL_MEMORY_HANDLE as PYROWAVE_EXTERNAL_MEMORY, ExternalPlane, PyroDecoder,
    available as pyrowave_available,
};

#[cfg(target_os = "android")]
mod android;
#[cfg(target_os = "android")]
pub use android::{
    DecodedFrame, Decoder, EXTERNAL_MEMORY_HANDLE as PYROWAVE_EXTERNAL_MEMORY, ExternalPlane,
    Frame, PyroDecoder, available as pyrowave_available,
};

/// Route FFmpeg's log output through `tracing` at a quiet level.
pub fn init_logging() {
    #[cfg(not(target_os = "android"))]
    unsafe {
        ff::av_log_set_level(ff::AV_LOG_ERROR)
    };
}
