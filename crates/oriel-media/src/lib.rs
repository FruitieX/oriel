//! Video encoding and decoding on top of FFmpeg (raw `ffmpeg-sys-next`).
//!
//! The encoder takes either CPU BGRA frames or, on Windows, D3D11 textures
//! (zero-copy into NVENC/AMF). The decoder prefers the platform's hardware
//! decoder and hands out CPU-side YUV frames.

pub use ffmpeg_sys_next as ff;

mod decoder;
mod encoder;
pub mod pyrowave;
mod util;

pub use decoder::{ColorMatrix, DecodedFrame, Decoder, PlaneFormat};
pub use encoder::{
    EncodedFrame, Encoder, EncoderConfig, EncoderInput, HwDevice, available_codecs,
    available_encoders,
};
pub use pyrowave::{
    EXTERNAL_MEMORY_HANDLE as PYROWAVE_EXTERNAL_MEMORY, ExternalPlane, PyroDecoder,
    available as pyrowave_available,
};

/// Route FFmpeg's log output through `tracing` at a quiet level.
pub fn init_logging() {
    unsafe { ff::av_log_set_level(ff::AV_LOG_ERROR) };
}
