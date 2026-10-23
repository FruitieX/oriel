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
    /// Each plane's bytes and row stride.
    planes: Vec<(Vec<u8>, usize)>,
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
        (&self.planes[i].0, self.planes[i].1)
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

    /// Copy a YUV_420_888 image out: NV12 row by row when the chroma is
    /// interleaved (what hardware decoders produce), else I420 sample by
    /// sample.
    fn copy(&self, image: &Image) -> Result<DecodedFrame> {
        let err = |e| anyhow::anyhow!("read image: {e:?}");
        let (w, h) = (
            self.size.0.min(image.width().map_err(err)? as u32),
            self.size.1.min(image.height().map_err(err)? as u32),
        );
        let (cw, ch) = (w.div_ceil(2) as usize, h.div_ceil(2) as usize);
        let rows = |data: &[u8], stride: usize, width: usize, height: usize| {
            let mut out = vec![0u8; width * height];
            for y in 0..height {
                let src = &data[(y * stride).min(data.len())..];
                let n = width.min(src.len());
                out[y * width..y * width + n].copy_from_slice(&src[..n]);
            }
            out
        };
        let luma = rows(
            image.plane_data(0).map_err(err)?,
            image.plane_row_stride(0).map_err(err)? as usize,
            w as usize,
            h as usize,
        );
        let (u, v) = (
            image.plane_data(1).map_err(err)?,
            image.plane_data(2).map_err(err)?,
        );
        let (u_px, v_px) = (
            image.plane_pixel_stride(1).map_err(err)?,
            image.plane_pixel_stride(2).map_err(err)?,
        );
        let (u_row, v_row) = (
            image.plane_row_stride(1).map_err(err)? as usize,
            image.plane_row_stride(2).map_err(err)? as usize,
        );
        let matrix = if h > 576 {
            ColorMatrix::Bt709
        } else {
            ColorMatrix::Bt601
        };
        let frame = |planes, format| DecodedFrame {
            planes,
            width: w,
            height: h,
            format,
            matrix,
            full_range: false,
        };
        if u_px == 2 && v_px == 2 && u_row == v_row && v.as_ptr() == u.as_ptr().wrapping_add(1) {
            // NV12: the U plane's bytes run U V U V ...; its last V is the V
            // plane's last byte, one past the U slice.
            let len = (u.len() + 1).min((ch - 1) * u_row + cw * 2);
            // SAFETY: those bytes lie within the V plane, which follows.
            let uv = unsafe { std::slice::from_raw_parts(u.as_ptr(), len) };
            let chroma = rows(uv, u_row, cw * 2, ch);
            return Ok(frame(
                vec![(luma, w as usize), (chroma, cw * 2)],
                PlaneFormat::Nv12,
            ));
        }
        let gather = |data: &[u8], row: usize, pixel: usize| {
            let mut out = vec![0u8; cw * ch];
            for y in 0..ch {
                let src = &data[(y * row).min(data.len())..];
                for (x, d) in out[y * cw..(y + 1) * cw].iter_mut().enumerate() {
                    if let Some(v) = src.get(x * pixel) {
                        *d = *v;
                    }
                }
            }
            out
        };
        Ok(frame(
            vec![
                (luma, w as usize),
                (gather(u, u_row, u_px as usize), cw),
                (gather(v, v_row, v_px as usize), cw),
            ],
            PlaneFormat::Yuv420p,
        ))
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

// Encoding (the phone as a host): the screen is captured and encoded on
// the Java side (MediaProjection into a MediaCodec encoder's input
// surface); `share` passes the encoded frames to the host pipeline, whose
// "encoder" here just hands them on.

pub enum HwDevice {
    None,
}

pub struct EncoderConfig {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub device: HwDevice,
    pub encoder: Option<String>,
}

pub enum EncoderInput<'a> {
    Bgra {
        data: &'a [u8],
        stride: usize,
    },
    /// The frame the screen share encoded already.
    Prepared,
}

#[derive(Debug)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub encode_us: u32,
}

/// What this device encodes in hardware.
pub fn available_codecs() -> Vec<Codec> {
    vec![Codec::Hevc, Codec::H264]
}

pub struct Encoder {
    _codec: Codec,
}

impl Encoder {
    pub fn open(config: &EncoderConfig) -> Result<Self> {
        if !matches!(config.codec, Codec::Hevc | Codec::H264) {
            bail!(
                "{} encoding is not available on Android",
                config.codec.name()
            );
        }
        share::clear();
        share::control()
            .context("the screen is not being shared")?
            .start(
                config.codec,
                config.width,
                config.height,
                config.fps,
                config.bitrate_kbps,
            )?;
        Ok(Self {
            _codec: config.codec,
        })
    }

    pub fn wants_cpu_input(&self) -> bool {
        false
    }

    pub fn set_bitrate(&mut self, kbps: u32) -> bool {
        if let Some(c) = share::control() {
            c.bitrate(kbps);
        }
        true
    }

    pub fn encode(
        &mut self,
        _input: EncoderInput<'_>,
        keyframe: bool,
    ) -> Result<Option<EncodedFrame>> {
        if keyframe && let Some(c) = share::control() {
            c.keyframe();
        }
        Ok(share::pop())
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        if let Some(c) = share::control() {
            c.stop();
        }
    }
}

pub mod share {
    //! The bridge between the Java screen share and the host pipeline.

    use std::{
        collections::VecDeque,
        sync::{Arc, Condvar, Mutex},
        time::Duration,
    };

    use oriel_proto::Codec;

    use super::EncodedFrame;

    /// Implemented by the app (over JNI).
    pub trait Control: Send + Sync {
        /// Start encoding the shared screen.
        fn start(
            &self,
            codec: Codec,
            width: u32,
            height: u32,
            fps: u32,
            kbps: u32,
        ) -> anyhow::Result<()>;
        fn stop(&self);
        fn keyframe(&self);
        fn bitrate(&self, kbps: u32);
    }

    #[derive(Default)]
    struct Queue {
        frames: VecDeque<EncodedFrame>,
        /// Frames were dropped: wait for a keyframe.
        need_key: bool,
        size: Option<(u32, u32)>,
    }

    static CONTROL: Mutex<Option<Arc<dyn Control>>> = Mutex::new(None);
    static QUEUE: Mutex<Option<Queue>> = Mutex::new(None);
    static READY: Condvar = Condvar::new();
    /// More than this waiting: the host fell behind.
    const MAX_QUEUED: usize = 8;

    fn with<T>(f: impl FnOnce(&mut Queue) -> T) -> T {
        f(QUEUE.lock().unwrap().get_or_insert_with(Default::default))
    }

    pub fn set_control(control: Option<Arc<dyn Control>>) {
        *CONTROL.lock().unwrap() = control;
    }

    pub fn control() -> Option<Arc<dyn Control>> {
        CONTROL.lock().unwrap().clone()
    }

    /// The shared screen's size (`None` while not shared).
    pub fn set_screen_size(size: Option<(u32, u32)>) {
        with(|q| q.size = size);
        READY.notify_all();
    }

    pub fn screen_size() -> Option<(u32, u32)> {
        with(|q| q.size)
    }

    /// An encoded frame from the screen share (codec configuration
    /// prepended to keyframes).
    pub fn push(data: Vec<u8>, keyframe: bool, encode_us: u32) {
        let mut ask_key = false;
        with(|q| {
            if q.need_key && !keyframe {
                return;
            }
            q.need_key = false;
            if q.frames.len() >= MAX_QUEUED {
                q.frames.clear();
                if !keyframe {
                    q.need_key = true;
                    ask_key = true;
                    return;
                }
            }
            q.frames.push_back(EncodedFrame {
                data,
                keyframe,
                encode_us,
            });
        });
        if ask_key && let Some(c) = control() {
            c.keyframe();
        }
        READY.notify_all();
    }

    /// Wait up to `timeout` for a frame.
    pub fn wait(timeout: Duration) -> bool {
        let guard = QUEUE.lock().unwrap();
        let (guard, _) = READY
            .wait_timeout_while(guard, timeout, |q| {
                q.as_ref()
                    .is_none_or(|q| q.frames.is_empty() && q.size.is_some())
            })
            .unwrap();
        guard.as_ref().is_some_and(|q| !q.frames.is_empty())
    }

    pub fn pop() -> Option<EncodedFrame> {
        with(|q| q.frames.pop_front())
    }

    pub fn clear() {
        with(|q| {
            q.frames.clear();
            q.need_key = false;
        });
    }
}
