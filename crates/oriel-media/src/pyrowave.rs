//! PyroWave (https://github.com/Themaister/pyrowave): an intra-only GPU
//! wavelet codec for very high bandwidth links, loaded at runtime from its
//! C API library. Hosts and clients without the library simply do not
//! offer the codec.

use std::{
    ffi::{c_int, c_void},
    path::PathBuf,
    ptr,
    sync::OnceLock,
    time::Instant,
};

use anyhow::{Result, anyhow, bail};

use crate::{DecodedFrame, EncodedFrame, PlaneFormat, decoder::ColorMatrix, ff, util::check};

type Handle = *mut c_void;

#[repr(C)]
struct CreateInfo {
    device: Handle,
    width: c_int,
    height: c_int,
    chroma: c_int,
}

#[repr(C)]
struct DecoderCreateInfo {
    device: Handle,
    width: c_int,
    height: c_int,
    chroma: c_int,
    fragment_path: bool,
}

#[repr(C)]
struct CpuBuffer {
    data: [*mut c_void; 3],
    row_stride_in_bytes: [usize; 3],
    plane_size_in_bytes: [usize; 3],
    width: c_int,
    height: c_int,
    format: c_int,
}

#[repr(C)]
struct RateControl {
    maximum_bitstream_size: usize,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Packet {
    offset: usize,
    size: usize,
}

const CHROMA_420: c_int = 0;
const FORMAT_YUV420P: c_int = 1;
const SUCCESS: c_int = 0;

struct Api {
    _lib: libloading::Library,
    path: PathBuf,
    create_default_device: unsafe extern "C" fn(*mut Handle) -> c_int,
    device_destroy: unsafe extern "C" fn(Handle),
    encoder_create: unsafe extern "C" fn(*const CreateInfo, *mut Handle) -> c_int,
    encoder_encode_cpu: unsafe extern "C" fn(Handle, *const CpuBuffer, *const RateControl) -> c_int,
    encoder_compute_num_packets: unsafe extern "C" fn(Handle, usize, *mut usize) -> c_int,
    encoder_packetize:
        unsafe extern "C" fn(Handle, *mut Packet, usize, *mut usize, *mut c_void, usize) -> c_int,
    encoder_destroy: unsafe extern "C" fn(Handle),
    decoder_create: unsafe extern "C" fn(*const DecoderCreateInfo, *mut Handle) -> c_int,
    decoder_push_packet: unsafe extern "C" fn(Handle, *const c_void, usize) -> c_int,
    decoder_decode_is_ready: unsafe extern "C" fn(Handle, bool) -> bool,
    decoder_decode_cpu_buffer_synchronous: unsafe extern "C" fn(Handle, *const CpuBuffer) -> c_int,
    decoder_destroy: unsafe extern "C" fn(Handle),
}

fn library_candidates() -> Vec<PathBuf> {
    let name = if cfg!(windows) {
        "libpyrowave-shared-1.dll"
    } else if cfg!(target_os = "macos") {
        "libpyrowave-shared.1.dylib"
    } else {
        "libpyrowave-shared.so.1"
    };
    let mut out = Vec::new();
    if let Some(p) = std::env::var_os("ORIEL_PYROWAVE_LIB") {
        out.push(PathBuf::from(p));
    }
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|p| p.to_path_buf()))
    {
        out.push(dir.join(name));
    }
    // Let the system loader search (LD_LIBRARY_PATH etc.).
    out.push(PathBuf::from(name));
    out
}

fn load() -> Result<Api, String> {
    let mut errors = Vec::new();
    for path in library_candidates() {
        let lib = match unsafe { libloading::Library::new(&path) } {
            Ok(l) => l,
            Err(e) => {
                errors.push(format!("{}: {e}", path.display()));
                continue;
            }
        };
        macro_rules! sym {
            ($name:literal) => {
                *unsafe { lib.get(concat!($name, "\0").as_bytes()) }
                    .map_err(|e| format!("{}: {e}", $name))?
            };
        }
        return Ok(Api {
            create_default_device: sym!("pyrowave_create_default_device"),
            device_destroy: sym!("pyrowave_device_destroy"),
            encoder_create: sym!("pyrowave_encoder_create"),
            encoder_encode_cpu: sym!("pyrowave_encoder_encode_cpu"),
            encoder_compute_num_packets: sym!("pyrowave_encoder_compute_num_packets"),
            encoder_packetize: sym!("pyrowave_encoder_packetize"),
            encoder_destroy: sym!("pyrowave_encoder_destroy"),
            decoder_create: sym!("pyrowave_decoder_create"),
            decoder_push_packet: sym!("pyrowave_decoder_push_packet"),
            decoder_decode_is_ready: sym!("pyrowave_decoder_decode_is_ready"),
            decoder_decode_cpu_buffer_synchronous: sym!(
                "pyrowave_decoder_decode_cpu_buffer_synchronous"
            ),
            decoder_destroy: sym!("pyrowave_decoder_destroy"),
            _lib: lib,
            path,
        });
    }
    Err(format!(
        "PyroWave library not found ({})",
        errors.join("; ")
    ))
}

fn api() -> Result<&'static Api> {
    static API: OnceLock<Result<Api, String>> = OnceLock::new();
    API.get_or_init(|| {
        let api = load();
        if let Ok(a) = &api {
            tracing::info!(path = %a.path.display(), "loaded PyroWave");
        }
        api
    })
    .as_ref()
    .map_err(|e| anyhow!("{e}"))
}

/// Whether the PyroWave library can be loaded.
pub fn available() -> bool {
    api().is_ok()
}

fn pw(code: c_int, what: &str) -> Result<()> {
    if code == SUCCESS {
        Ok(())
    } else {
        bail!("{what} failed (PyroWave error {code})")
    }
}

struct Device(Handle);

impl Device {
    fn new() -> Result<Self> {
        let api = api()?;
        let mut device = ptr::null_mut();
        pw(
            unsafe { (api.create_default_device)(&mut device) },
            "create PyroWave Vulkan device",
        )?;
        Ok(Self(device))
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        if let Ok(api) = api() {
            unsafe { (api.device_destroy)(self.0) };
        }
    }
}

/// Allocate a YUV420P frame (BT.709 full range, which PyroWave's float
/// pipeline preserves best).
unsafe fn yuv_frame(w: c_int, h: c_int) -> Result<*mut ff::AVFrame> {
    unsafe {
        let mut frame = ff::av_frame_alloc();
        (*frame).format = ff::AVPixelFormat::AV_PIX_FMT_YUV420P as c_int;
        (*frame).width = w;
        (*frame).height = h;
        (*frame).colorspace = ff::AVColorSpace::AVCOL_SPC_BT709;
        (*frame).color_range = ff::AVColorRange::AVCOL_RANGE_JPEG;
        let ret = ff::av_frame_get_buffer(frame, 0);
        if ret < 0 {
            ff::av_frame_free(&mut frame);
            check(ret, "av_frame_get_buffer")?;
        }
        Ok(frame)
    }
}

unsafe fn cpu_buffer(frame: *mut ff::AVFrame) -> CpuBuffer {
    unsafe {
        let (w, h) = ((*frame).width, (*frame).height);
        let rows = [
            h as usize,
            (h as usize).div_ceil(2),
            (h as usize).div_ceil(2),
        ];
        let mut b = CpuBuffer {
            data: [ptr::null_mut(); 3],
            row_stride_in_bytes: [0; 3],
            plane_size_in_bytes: [0; 3],
            width: w,
            height: h,
            format: FORMAT_YUV420P,
        };
        for (i, rows) in rows.into_iter().enumerate() {
            b.data[i] = (*frame).data[i].cast();
            b.row_stride_in_bytes[i] = (*frame).linesize[i] as usize;
            b.plane_size_in_bytes[i] = (*frame).linesize[i] as usize * rows;
        }
        b
    }
}

/// Packets inside an encoded frame: `u32` little-endian length, then bytes.
const PACKET_BOUNDARY: usize = 64 * 1024;

pub struct PyroEncoder {
    encoder: Handle,
    _device: Device,
    sws: *mut ff::SwsContext,
    frame: *mut ff::AVFrame,
    max_bytes: usize,
    fps: u32,
    bitstream: Vec<u8>,
    packets: Vec<Packet>,
}

unsafe impl Send for PyroEncoder {}

impl PyroEncoder {
    pub fn new(width: u32, height: u32, fps: u32, bitrate_kbps: u32) -> Result<Self> {
        if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            bail!("PyroWave 4:2:0 needs an even size ({width}x{height})");
        }
        let api = api()?;
        let device = Device::new()?;
        let mut encoder = ptr::null_mut();
        let info = CreateInfo {
            device: device.0,
            width: width as c_int,
            height: height as c_int,
            chroma: CHROMA_420,
        };
        pw(
            unsafe { (api.encoder_create)(&info, &mut encoder) },
            "create PyroWave encoder",
        )?;
        let mut enc = Self {
            encoder,
            _device: device,
            sws: ptr::null_mut(),
            frame: ptr::null_mut(),
            max_bytes: 0,
            fps: fps.max(1),
            bitstream: Vec::new(),
            packets: Vec::new(),
        };
        unsafe {
            enc.frame = yuv_frame(width as c_int, height as c_int)?;
            enc.sws = ff::sws_getContext(
                width as c_int,
                height as c_int,
                ff::AVPixelFormat::AV_PIX_FMT_BGRA,
                width as c_int,
                height as c_int,
                ff::AVPixelFormat::AV_PIX_FMT_YUV420P,
                ff::SwsFlags::SWS_POINT as c_int,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
            );
            if enc.sws.is_null() {
                bail!("sws_getContext failed");
            }
            let coeffs = ff::sws_getCoefficients(ff::SWS_CS_ITU709);
            ff::sws_setColorspaceDetails(enc.sws, coeffs, 1, coeffs, 1, 0, 1 << 16, 1 << 16);
        }
        enc.set_bitrate(bitrate_kbps);
        Ok(enc)
    }

    pub fn set_bitrate(&mut self, kbps: u32) {
        self.max_bytes = (kbps as usize * 1000 / 8 / self.fps as usize).max(16 * 1024);
        self.bitstream.resize(self.max_bytes + 64 * 1024, 0);
    }

    pub fn encode_bgra(&mut self, data: &[u8], stride: usize) -> Result<EncodedFrame> {
        let started = Instant::now();
        let api = api()?;
        unsafe {
            let h = (*self.frame).height;
            if data.len() < stride * h as usize {
                bail!("frame buffer too small");
            }
            let src = [data.as_ptr(), ptr::null(), ptr::null(), ptr::null()];
            let src_stride = [stride as c_int, 0, 0, 0];
            ff::sws_scale(
                self.sws,
                src.as_ptr(),
                src_stride.as_ptr(),
                0,
                h,
                (*self.frame).data.as_ptr(),
                (*self.frame).linesize.as_ptr(),
            );
            let buffers = cpu_buffer(self.frame);
            let rate = RateControl {
                maximum_bitstream_size: self.max_bytes,
            };
            pw(
                (api.encoder_encode_cpu)(self.encoder, &buffers, &rate),
                "PyroWave encode",
            )?;
            let mut count = 0usize;
            pw(
                (api.encoder_compute_num_packets)(self.encoder, PACKET_BOUNDARY, &mut count),
                "PyroWave packet count",
            )?;
            self.packets.resize(count, Packet::default());
            let mut written = 0usize;
            pw(
                (api.encoder_packetize)(
                    self.encoder,
                    self.packets.as_mut_ptr(),
                    PACKET_BOUNDARY,
                    &mut written,
                    self.bitstream.as_mut_ptr().cast(),
                    self.bitstream.len(),
                ),
                "PyroWave packetize",
            )?;
            let mut out = Vec::with_capacity(self.max_bytes + written * 4);
            for p in &self.packets[..written] {
                out.extend_from_slice(&(p.size as u32).to_le_bytes());
                out.extend_from_slice(&self.bitstream[p.offset..p.offset + p.size]);
            }
            Ok(EncodedFrame {
                data: out,
                keyframe: true,
                encode_us: started.elapsed().as_micros() as u32,
            })
        }
    }
}

impl Drop for PyroEncoder {
    fn drop(&mut self) {
        unsafe {
            if let Ok(api) = api() {
                (api.encoder_destroy)(self.encoder);
            }
            ff::sws_freeContext(self.sws);
            ff::av_frame_free(&mut self.frame);
        }
    }
}

pub struct PyroDecoder {
    decoder: Handle,
    _device: Device,
    size: (u32, u32),
}

unsafe impl Send for PyroDecoder {}

impl PyroDecoder {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        let api = api()?;
        let device = Device::new()?;
        let mut decoder = ptr::null_mut();
        let info = DecoderCreateInfo {
            device: device.0,
            width: width as c_int,
            height: height as c_int,
            chroma: CHROMA_420,
            fragment_path: false,
        };
        pw(
            unsafe { (api.decoder_create)(&info, &mut decoder) },
            "create PyroWave decoder",
        )?;
        Ok(Self {
            decoder,
            _device: device,
            size: (width, height),
        })
    }

    pub fn decode(&mut self, data: &[u8]) -> Result<Option<DecodedFrame>> {
        let api = api()?;
        let mut rest = data;
        while rest.len() >= 4 {
            let len = u32::from_le_bytes(rest[..4].try_into().unwrap()) as usize;
            if rest.len() < 4 + len {
                bail!("truncated PyroWave packet");
            }
            pw(
                unsafe { (api.decoder_push_packet)(self.decoder, rest[4..].as_ptr().cast(), len) },
                "PyroWave push packet",
            )?;
            rest = &rest[4 + len..];
        }
        if !unsafe { (api.decoder_decode_is_ready)(self.decoder, false) } {
            return Ok(None);
        }
        unsafe {
            let frame = yuv_frame(self.size.0 as c_int, self.size.1 as c_int)?;
            let decoded =
                DecodedFrame::from_raw(frame, PlaneFormat::Yuv420p, ColorMatrix::Bt709, true);
            let buffers = cpu_buffer(frame);
            pw(
                (api.decoder_decode_cpu_buffer_synchronous)(self.decoder, &buffers),
                "PyroWave decode",
            )?;
            Ok(Some(decoded))
        }
    }
}

impl Drop for PyroDecoder {
    fn drop(&mut self) {
        if let Ok(api) = api() {
            unsafe { (api.decoder_destroy)(self.decoder) };
        }
    }
}
