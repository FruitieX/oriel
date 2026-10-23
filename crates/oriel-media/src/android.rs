//! Decoding on Android: MediaCodec, rendering into an `AImageReader` whose
//! YUV planes are copied out for the renderer. Hardware decoders hold
//! frames back unless asked not to, so the low-latency switches (standard
//! and vendor ones) are set where the decoder accepts them.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use ndk::{
    hardware_buffer::HardwareBufferUsage,
    media::{
        image_reader::{AcquireResult, Image, ImageFormat, ImageReader},
        media_codec::{
            DequeuedInputBufferResult, DequeuedOutputBufferInfoResult, MediaCodec,
            MediaCodecDirection,
        },
        media_format::MediaFormat,
    },
    native_window::NativeWindow,
};
use oriel_proto::Codec;

use crate::{ColorMatrix, PlaneFormat};

/// A decoded picture in CPU memory (tightly packed planes).
pub struct DecodedFrame {
    planes: Vec<Vec<u8>>,
    width: u32,
    height: u32,
    pub format: PlaneFormat,
    pub matrix: ColorMatrix,
    pub full_range: bool,
}

impl DecodedFrame {
    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn plane_count(&self) -> usize {
        self.planes.len()
    }

    /// Size of plane `i` in samples.
    pub fn plane_size(&self, i: usize) -> (u32, u32) {
        match i {
            0 => (self.width, self.height),
            _ => (self.width.div_ceil(2), self.height.div_ceil(2)),
        }
    }

    /// Bytes of plane `i` and its row stride.
    pub fn plane(&self, i: usize) -> (&[u8], usize) {
        (&self.planes[i], self.plane_size(i).0 as usize)
    }
}

pub enum Frame {
    Cpu(DecodedFrame),
}

pub struct Decoder {
    codec: MediaCodec,
    reader: ImageReader,
    _window: NativeWindow,
    description: String,
    size: (u32, u32),
    /// Presentation timestamps, only to keep them increasing.
    pts: u64,
    started: bool,
}

// MediaCodec and AImageReader may be used from any one thread at a time.
unsafe impl Send for Decoder {}

fn mime(codec: Codec) -> Result<&'static str> {
    Ok(match codec {
        Codec::H264 => "video/avc",
        Codec::Hevc => "video/hevc",
        Codec::Av1 => "video/av01",
        Codec::PyroWave => bail!("PyroWave is not available on Android"),
    })
}

/// Low-latency switches: the standard one (Android 11) and the vendors'.
const LOW_LATENCY_KEYS: [&str; 5] = [
    "low-latency",
    "vendor.qti-ext-dec-low-latency.enable",
    "vendor.qti-ext-dec-picture-order.enable",
    "vendor.rtc-ext-dec-low-latency.enable",
    "vendor.low-latency.enable",
];

impl Decoder {
    pub fn open(codec: Codec, width: u32, height: u32, _hardware: bool) -> Result<Self> {
        let mime = mime(codec)?;
        let reader = ImageReader::new_with_usage(
            width as i32,
            height as i32,
            ImageFormat::YUV_420_888,
            HardwareBufferUsage::CPU_READ_OFTEN,
            4,
        )
        .context("create the image reader")?;
        let window = reader.window().context("image reader surface")?;
        let format = |low_latency: bool| {
            let mut f = MediaFormat::new();
            f.set_str("mime", mime);
            f.set_i32("width", width as i32);
            f.set_i32("height", height as i32);
            // Keyframes are large; the default input buffer can be too small.
            f.set_i32(
                "max-input-size",
                (width * height * 3 / 2).max(1 << 20) as i32,
            );
            // Realtime priority.
            f.set_i32("priority", 0);
            if low_latency {
                for key in LOW_LATENCY_KEYS {
                    f.set_i32(key, 1);
                }
            }
            f
        };
        let decoder = MediaCodec::from_decoder_type(mime)
            .with_context(|| format!("no {} decoder", codec.name()))?;
        let mut low_latency = true;
        if let Err(e) =
            decoder.configure(&format(true), Some(&window), MediaCodecDirection::Decoder)
        {
            tracing::info!("decoder refused the low-latency options ({e:?}); without them");
            low_latency = false;
            decoder
                .configure(&format(false), Some(&window), MediaCodecDirection::Decoder)
                .map_err(|e| anyhow::anyhow!("configure the decoder: {e:?}"))?;
        }
        decoder
            .start()
            .map_err(|e| anyhow::anyhow!("start the decoder: {e:?}"))?;
        let name = decoder.name().unwrap_or_else(|_| "MediaCodec".into());
        let description = format!(
            "{name} (MediaCodec{})",
            if low_latency { ", low latency" } else { "" }
        );
        Ok(Self {
            codec: decoder,
            reader,
            _window: window,
            description,
            size: (width, height),
            pts: 0,
            started: true,
        })
    }

    pub fn open_with(
        codec: Codec,
        width: u32,
        height: u32,
        hardware: bool,
        _export_dmabuf: bool,
    ) -> Result<Self> {
        Self::open(codec, width, height, hardware)
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn decode_frame(&mut self, data: &[u8]) -> Result<Option<Frame>> {
        Ok(self.decode(data)?.map(Frame::Cpu))
    }

    /// Decode one frame's data. Returns the newest frame it produced.
    pub fn decode(&mut self, data: &[u8]) -> Result<Option<DecodedFrame>> {
        match self
            .codec
            .dequeue_input_buffer(Duration::from_millis(50))
            .map_err(|e| anyhow::anyhow!("dequeue input: {e:?}"))?
        {
            DequeuedInputBufferResult::Buffer(mut buffer) => {
                let dst = buffer.buffer_mut();
                if data.len() > dst.len() {
                    bail!(
                        "frame of {} bytes exceeds the decoder's input buffer",
                        data.len()
                    );
                }
                // SAFETY: MaybeUninit<u8> has u8's layout; `data` fits.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        data.as_ptr(),
                        dst.as_mut_ptr().cast::<u8>(),
                        data.len(),
                    );
                }
                self.pts += 1000;
                self.codec
                    .queue_input_buffer(buffer, 0, data.len(), self.pts, 0)
                    .map_err(|e| anyhow::anyhow!("queue input: {e:?}"))?;
            }
            DequeuedInputBufferResult::TryAgainLater => bail!("the decoder is not taking input"),
        }

        // Low-latency decoders answer within a few milliseconds; others a
        // frame later, which the next call picks up.
        let mut rendered = false;
        let mut wait = Duration::from_millis(30);
        loop {
            match self
                .codec
                .dequeue_output_buffer(wait)
                .map_err(|e| anyhow::anyhow!("dequeue output: {e:?}"))?
            {
                DequeuedOutputBufferInfoResult::Buffer(out) => {
                    self.codec
                        .release_output_buffer(out, true)
                        .map_err(|e| anyhow::anyhow!("render output: {e:?}"))?;
                    rendered = true;
                    wait = Duration::ZERO;
                }
                DequeuedOutputBufferInfoResult::OutputFormatChanged
                | DequeuedOutputBufferInfoResult::OutputBuffersChanged => {}
                DequeuedOutputBufferInfoResult::TryAgainLater => break,
            }
        }
        if !rendered {
            return Ok(None);
        }
        // The rendered picture reaches the reader shortly after.
        let deadline = Instant::now() + Duration::from_millis(30);
        loop {
            match self
                .reader
                .acquire_latest_image()
                .map_err(|e| anyhow::anyhow!("acquire image: {e:?}"))?
            {
                AcquireResult::Image(image) => return Ok(Some(self.copy(&image)?)),
                _ if Instant::now() >= deadline => return Ok(None),
                _ => std::thread::sleep(Duration::from_micros(500)),
            }
        }
    }

    /// Copy a YUV_420_888 image into tightly packed I420 planes.
    fn copy(&self, image: &Image) -> Result<DecodedFrame> {
        let err = |e| anyhow::anyhow!("read image: {e:?}");
        let (w, h) = (
            self.size.0.min(image.width().map_err(err)? as u32),
            self.size.1.min(image.height().map_err(err)? as u32),
        );
        let mut planes = Vec::with_capacity(3);
        for i in 0..3 {
            let (pw, ph) = if i == 0 {
                (w as usize, h as usize)
            } else {
                (w.div_ceil(2) as usize, h.div_ceil(2) as usize)
            };
            let data = image.plane_data(i).map_err(err)?;
            let row = image.plane_row_stride(i).map_err(err)? as usize;
            let pixel = image.plane_pixel_stride(i).map_err(err)? as usize;
            let mut out = vec![0u8; pw * ph];
            for y in 0..ph {
                let src = &data[(y * row).min(data.len())..];
                let dst = &mut out[y * pw..(y + 1) * pw];
                if pixel == 1 {
                    let n = pw.min(src.len());
                    dst[..n].copy_from_slice(&src[..n]);
                } else {
                    for (x, d) in dst.iter_mut().enumerate() {
                        if let Some(v) = src.get(x * pixel) {
                            *d = *v;
                        }
                    }
                }
            }
            planes.push(out);
        }
        Ok(DecodedFrame {
            planes,
            width: w,
            height: h,
            format: PlaneFormat::Yuv420p,
            matrix: if h > 576 {
                ColorMatrix::Bt709
            } else {
                ColorMatrix::Bt601
            },
            full_range: false,
        })
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        if self.started {
            let _ = self.codec.stop();
        }
    }
}

/// PyroWave needs desktop Vulkan interop; not on Android.
pub struct PyroDecoder;

pub struct ExternalPlane {
    pub handle: usize,
    pub width: u32,
    pub height: u32,
    pub info: ash::vk::ImageCreateInfo<'static>,
}

pub const EXTERNAL_MEMORY_HANDLE: ash::vk::ExternalMemoryHandleTypeFlags =
    ash::vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD;

impl PyroDecoder {
    pub fn new_on_gpu(
        _width: u32,
        _height: u32,
        _device_uuid: [u8; 16],
        _driver_uuid: [u8; 16],
    ) -> Result<Self> {
        bail!("PyroWave is not available on Android")
    }

    pub fn import_planes(&mut self, _planes: [ExternalPlane; 3]) -> Result<usize> {
        bail!("PyroWave is not available on Android")
    }

    pub fn decode_into(&mut self, _data: &[u8], _set: usize) -> Result<bool> {
        bail!("PyroWave is not available on Android")
    }
}

pub fn available() -> bool {
    false
}
