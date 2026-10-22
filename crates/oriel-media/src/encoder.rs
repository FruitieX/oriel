use std::{
    ffi::{CString, c_int},
    ptr,
    time::Instant,
};

#[cfg(windows)]
use std::ffi::c_void;

use anyhow::{Result, anyhow, bail};
use oriel_proto::Codec;

use crate::{
    ff,
    util::{Dict, check, is_eagain},
};

/// A device frames can be handed to the encoder on without a CPU copy.
#[derive(Clone, Copy, Debug)]
pub enum HwDevice {
    None,
    /// An `ID3D11Device*`. The encoder takes over one reference: AddRef it
    /// before passing it in.
    #[cfg(windows)]
    D3D11(*mut c_void),
}

unsafe impl Send for HwDevice {}

#[derive(Clone, Debug)]
pub struct EncoderConfig {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub device: HwDevice,
    /// Only try this FFmpeg encoder (e.g. `hevc_nvenc`).
    pub encoder: Option<String>,
}

pub enum EncoderInput<'a> {
    /// Packed BGRA (or BGRX) pixels.
    Bgra { data: &'a [u8], stride: usize },
    /// Zero-copy input: the callback copies the frame into the given
    /// `ID3D11Texture2D*` at array slice `index` (BGRA, same size).
    #[cfg(windows)]
    D3D11 {
        copy: &'a mut dyn FnMut(*mut c_void, u32) -> Result<()>,
    },
}

#[derive(Debug)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub encode_us: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Nvenc,
    Amf,
    Qsv,
    MediaFoundation,
    Vaapi,
    VideoToolbox,
    X264,
    X265,
    SvtAv1,
}

fn candidates(codec: Codec) -> Vec<(&'static str, Kind)> {
    use Kind::*;
    let (nv, amf, qsv, mf, va, vt, sw): (_, _, _, _, _, _, Option<(&str, Kind)>) = match codec {
        Codec::H264 => (
            "h264_nvenc",
            "h264_amf",
            "h264_qsv",
            Some("h264_mf"),
            "h264_vaapi",
            "h264_videotoolbox",
            Some(("libx264", X264)),
        ),
        Codec::Hevc => (
            "hevc_nvenc",
            "hevc_amf",
            "hevc_qsv",
            Some("hevc_mf"),
            "hevc_vaapi",
            "hevc_videotoolbox",
            Some(("libx265", X265)),
        ),
        Codec::Av1 => (
            "av1_nvenc",
            "av1_amf",
            "av1_qsv",
            None,
            "av1_vaapi",
            "av1_videotoolbox",
            Some(("libsvtav1", SvtAv1)),
        ),
        Codec::PyroWave => return vec![],
    };
    let mut out = Vec::new();
    if cfg!(windows) {
        out.extend([(nv, Nvenc), (amf, Amf), (qsv, Qsv)]);
        if let Some(mf) = mf {
            out.push((mf, MediaFoundation));
        }
    } else if cfg!(target_os = "macos") {
        out.push((vt, VideoToolbox));
    } else {
        out.extend([(va, Vaapi), (nv, Nvenc)]);
    }
    out.extend(sw);
    out
}

/// Mirrors `AVD3D11VADeviceContext` (FFmpeg 8.x).
#[cfg(windows)]
#[repr(C)]
struct AVD3D11VADeviceContext {
    device: *mut c_void,
    device_context: *mut c_void,
    video_device: *mut c_void,
    video_context: *mut c_void,
    lock: Option<unsafe extern "C" fn(*mut c_void)>,
    unlock: Option<unsafe extern "C" fn(*mut c_void)>,
    lock_ctx: *mut c_void,
    bind_flags: u32,
    misc_flags: u32,
}

enum InputPath {
    /// Convert BGRA to `frame`'s format with swscale.
    Sws {
        sws: *mut ff::SwsContext,
        frame: *mut ff::AVFrame,
    },
    /// Convert to NV12 with swscale, then upload into a hardware surface.
    Upload {
        sws: *mut ff::SwsContext,
        sw_frame: *mut ff::AVFrame,
        frames: *mut ff::AVBufferRef,
    },
    /// Copy into a pooled D3D11 texture on the GPU.
    #[cfg(windows)]
    D3D11 { frames: *mut ff::AVBufferRef },
}

struct FfEncoder {
    ctx: *mut ff::AVCodecContext,
    pkt: *mut ff::AVPacket,
    input: InputPath,
    device: *mut ff::AVBufferRef,
    name: String,
    start: Instant,
    last_pts: i64,
}

unsafe impl Send for FfEncoder {}

impl FfEncoder {
    fn open(config: &EncoderConfig) -> Result<Self> {
        let mut errors = Vec::new();
        for (name, kind) in candidates(config.codec) {
            if config.encoder.as_deref().is_some_and(|only| only != name) {
                continue;
            }
            match unsafe { Self::open_named(config, name, kind) } {
                Ok(enc) => {
                    tracing::info!(
                        encoder = name,
                        width = config.width,
                        height = config.height,
                        fps = config.fps,
                        kbps = config.bitrate_kbps,
                        zero_copy = !enc.wants_cpu_input(),
                        "opened video encoder"
                    );
                    return Ok(enc);
                }
                Err(e) => {
                    tracing::debug!(encoder = name, "encoder unavailable: {e:#}");
                    errors.push(format!("{name}: {e:#}"));
                }
            }
        }
        bail!(
            "no {} encoder could be opened ({})",
            config.codec.name(),
            if errors.is_empty() {
                "none available".to_string()
            } else {
                errors.join("; ")
            }
        )
    }

    fn wants_cpu_input(&self) -> bool {
        match self.input {
            InputPath::Sws { .. } | InputPath::Upload { .. } => true,
            #[cfg(windows)]
            InputPath::D3D11 { .. } => false,
        }
    }

    unsafe fn open_named(config: &EncoderConfig, name: &str, kind: Kind) -> Result<Self> {
        let cname = CString::new(name).unwrap();
        let codec = unsafe { ff::avcodec_find_encoder_by_name(cname.as_ptr()) };
        if codec.is_null() {
            bail!("not built into FFmpeg");
        }
        let mut enc = FfEncoder {
            ctx: unsafe { ff::avcodec_alloc_context3(codec) },
            pkt: unsafe { ff::av_packet_alloc() },
            input: InputPath::Sws {
                sws: ptr::null_mut(),
                frame: ptr::null_mut(),
            },
            device: ptr::null_mut(),
            name: name.to_string(),
            start: Instant::now(),
            last_pts: -1,
        };
        if enc.ctx.is_null() || enc.pkt.is_null() {
            bail!("out of memory");
        }
        let (w, h) = (config.width as c_int, config.height as c_int);
        let fps = config.fps.max(1) as c_int;
        let bit_rate = config.bitrate_kbps.max(1000) as i64 * 1000;
        unsafe {
            let ctx = &mut *enc.ctx;
            ctx.width = w;
            ctx.height = h;
            ctx.time_base = ff::AVRational {
                num: 1,
                den: 1_000_000,
            };
            ctx.framerate = ff::AVRational { num: fps, den: 1 };
            ctx.bit_rate = bit_rate;
            ctx.rc_max_rate = bit_rate;
            // About two frames of VBV: small enough to keep latency down,
            // large enough that keyframes are not starved.
            ctx.rc_buffer_size = (bit_rate * 2 / fps as i64).min(i32::MAX as i64) as c_int;
            // Keyframes only on demand (stream start, loss recovery).
            ctx.gop_size = fps * 600;
            ctx.max_b_frames = 0;
            ctx.flags |= ff::AV_CODEC_FLAG_LOW_DELAY as c_int;
            ctx.color_primaries = ff::AVColorPrimaries::AVCOL_PRI_BT709;
            ctx.color_trc = ff::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
            ctx.colorspace = ff::AVColorSpace::AVCOL_SPC_BT709;
            ctx.color_range = ff::AVColorRange::AVCOL_RANGE_MPEG;
        }

        let sw_format = match kind {
            Kind::X264 | Kind::X265 | Kind::SvtAv1 => ff::AVPixelFormat::AV_PIX_FMT_YUV420P,
            _ => ff::AVPixelFormat::AV_PIX_FMT_NV12,
        };
        match (kind, config.device) {
            #[cfg(windows)]
            (Kind::Nvenc | Kind::Amf, HwDevice::D3D11(device)) => unsafe {
                enc.setup_d3d11(device, w, h)?
            },
            (Kind::Vaapi, _) => unsafe { enc.setup_vaapi(w, h)? },
            _ => unsafe { enc.setup_sws(sw_format, w, h)? },
        }

        let mut opts = Dict::new();
        match kind {
            Kind::Nvenc => {
                let preset = std::env::var("ORIEL_NVENC_PRESET").unwrap_or_else(|_| "p1".into());
                opts.set("preset", preset);
                opts.set("tune", "ull");
                opts.set("zerolatency", 1);
                opts.set("delay", 0);
                opts.set("rc", "cbr");
                opts.set("forced-idr", 1);
            }
            Kind::Amf => {
                opts.set("usage", "ultralowlatency");
                opts.set("quality", "speed");
                opts.set("rc", "cbr");
                opts.set("forced_idr", 1);
            }
            Kind::Qsv => {
                opts.set("preset", "veryfast");
                opts.set("async_depth", 1);
                opts.set("forced_idr", 1);
            }
            Kind::MediaFoundation => {
                opts.set("rate_control", "cbr");
                opts.set("scenario", "display_remoting");
                opts.set("hw_encoding", 1);
            }
            Kind::Vaapi => {
                opts.set("rc_mode", "CBR");
                opts.set("async_depth", 1);
            }
            Kind::VideoToolbox => {
                opts.set("realtime", 1);
                opts.set("prio_speed", 1);
            }
            Kind::X264 => {
                opts.set("preset", "ultrafast");
                opts.set("tune", "zerolatency");
                opts.set("forced-idr", 1);
            }
            Kind::X265 => {
                opts.set("preset", "ultrafast");
                opts.set("tune", "zerolatency");
                opts.set("forced-idr", 1);
                opts.set("x265-params", "log-level=error");
            }
            Kind::SvtAv1 => {
                opts.set("preset", 12);
                opts.set("svtav1-params", "pred-struct=1");
            }
        }
        check(
            unsafe { ff::avcodec_open2(enc.ctx, codec, &mut opts.0) },
            "avcodec_open2",
        )?;
        let unused = opts.leftover();
        if !unused.is_empty() {
            tracing::debug!(encoder = name, ?unused, "encoder ignored options");
        }
        Ok(enc)
    }

    unsafe fn setup_sws(&mut self, format: ff::AVPixelFormat, w: c_int, h: c_int) -> Result<()> {
        unsafe {
            (*self.ctx).pix_fmt = format;
            let frame = alloc_frame(format, w, h)?;
            let sws = bgra_sws(format, w, h)?;
            self.input = InputPath::Sws { sws, frame };
        }
        Ok(())
    }

    unsafe fn setup_vaapi(&mut self, w: c_int, h: c_int) -> Result<()> {
        unsafe {
            check(
                ff::av_hwdevice_ctx_create(
                    &mut self.device,
                    ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                    ptr::null(),
                    ptr::null_mut(),
                    0,
                ),
                "create VAAPI device",
            )?;
            let frames = hw_frames(
                self.device,
                ff::AVPixelFormat::AV_PIX_FMT_VAAPI,
                ff::AVPixelFormat::AV_PIX_FMT_NV12,
                w,
                h,
                4,
            )?;
            (*self.ctx).pix_fmt = ff::AVPixelFormat::AV_PIX_FMT_VAAPI;
            (*self.ctx).hw_frames_ctx = ff::av_buffer_ref(frames);
            let sw_frame = alloc_frame(ff::AVPixelFormat::AV_PIX_FMT_NV12, w, h)?;
            let sws = bgra_sws(ff::AVPixelFormat::AV_PIX_FMT_NV12, w, h)?;
            self.input = InputPath::Upload {
                sws,
                sw_frame,
                frames,
            };
        }
        Ok(())
    }

    #[cfg(windows)]
    unsafe fn setup_d3d11(&mut self, device: *mut c_void, w: c_int, h: c_int) -> Result<()> {
        unsafe {
            self.device = ff::av_hwdevice_ctx_alloc(ff::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA);
            if self.device.is_null() {
                bail!("av_hwdevice_ctx_alloc failed");
            }
            let hwdev = (*self.device).data as *mut ff::AVHWDeviceContext;
            let d3d = (*hwdev).hwctx as *mut AVD3D11VADeviceContext;
            // FFmpeg releases this reference when the device is freed. Add
            // our own so the caller's reference stays valid.
            com_add_ref(device);
            (*d3d).device = device;
            check(ff::av_hwdevice_ctx_init(self.device), "init D3D11 device")?;
            let frames = hw_frames(
                self.device,
                ff::AVPixelFormat::AV_PIX_FMT_D3D11,
                ff::AVPixelFormat::AV_PIX_FMT_BGRA,
                w,
                h,
                0,
            )?;
            (*self.ctx).pix_fmt = ff::AVPixelFormat::AV_PIX_FMT_D3D11;
            (*self.ctx).hw_frames_ctx = ff::av_buffer_ref(frames);
            (*self.ctx).hw_device_ctx = ff::av_buffer_ref(self.device);
            self.input = InputPath::D3D11 { frames };
        }
        Ok(())
    }

    /// Encode one frame. Returns `None` if the encoder produced no output
    /// for it (which low-delay encoders should not do).
    fn encode(&mut self, input: EncoderInput<'_>, keyframe: bool) -> Result<Option<EncodedFrame>> {
        let started = Instant::now();
        // Strictly increasing microsecond timestamps.
        let pts = (self.start.elapsed().as_micros() as i64).max(self.last_pts + 1);
        self.last_pts = pts;
        unsafe {
            let w = (*self.ctx).width;
            let h = (*self.ctx).height;
            let frame: *mut ff::AVFrame = match (&self.input, input) {
                (InputPath::Sws { sws, frame }, EncoderInput::Bgra { data, stride }) => {
                    check(ff::av_frame_make_writable(*frame), "av_frame_make_writable")?;
                    convert_bgra(*sws, data, stride, h, *frame)?;
                    ff::av_frame_clone(*frame)
                }
                (
                    InputPath::Upload {
                        sws,
                        sw_frame,
                        frames,
                    },
                    EncoderInput::Bgra { data, stride },
                ) => {
                    convert_bgra(*sws, data, stride, h, *sw_frame)?;
                    let hw = ff::av_frame_alloc();
                    check(ff::av_hwframe_get_buffer(*frames, hw, 0), "get hw frame")?;
                    let ret = ff::av_hwframe_transfer_data(hw, *sw_frame, 0);
                    if ret < 0 {
                        let mut hw = hw;
                        ff::av_frame_free(&mut hw);
                        check(ret, "upload frame")?;
                    }
                    hw
                }
                #[cfg(windows)]
                (InputPath::D3D11 { frames }, EncoderInput::D3D11 { copy }) => {
                    let hw = ff::av_frame_alloc();
                    check(ff::av_hwframe_get_buffer(*frames, hw, 0), "get D3D11 frame")?;
                    let texture = (*hw).data[0] as *mut c_void;
                    let index = (*hw).data[1] as usize as u32;
                    if let Err(e) = copy(texture, index) {
                        let mut hw = hw;
                        ff::av_frame_free(&mut hw);
                        return Err(e);
                    }
                    hw
                }
                #[cfg(windows)]
                _ => bail!("encoder {} got the wrong kind of input", self.name),
            };
            let _ = (w, h);
            (*frame).pts = pts;
            if keyframe {
                (*frame).pict_type = ff::AVPictureType::AV_PICTURE_TYPE_I;
                (*frame).flags |= ff::AV_FRAME_FLAG_KEY;
            } else {
                (*frame).pict_type = ff::AVPictureType::AV_PICTURE_TYPE_NONE;
                (*frame).flags &= !ff::AV_FRAME_FLAG_KEY;
            }
            let ret = ff::avcodec_send_frame(self.ctx, frame);
            let mut frame = frame;
            ff::av_frame_free(&mut frame);
            check(ret, "avcodec_send_frame")?;

            let mut out: Option<EncodedFrame> = None;
            loop {
                let ret = ff::avcodec_receive_packet(self.ctx, self.pkt);
                if is_eagain(ret) {
                    break;
                }
                check(ret, "avcodec_receive_packet")?;
                let pkt = &*self.pkt;
                let data = std::slice::from_raw_parts(pkt.data, pkt.size as usize);
                let is_key = pkt.flags & ff::AV_PKT_FLAG_KEY != 0;
                match &mut out {
                    Some(o) => {
                        o.data.extend_from_slice(data);
                        o.keyframe |= is_key;
                    }
                    None => {
                        out = Some(EncodedFrame {
                            data: data.to_vec(),
                            keyframe: is_key,
                            encode_us: 0,
                        })
                    }
                }
                ff::av_packet_unref(self.pkt);
            }
            if let Some(o) = &mut out {
                o.encode_us = started.elapsed().as_micros() as u32;
            }
            Ok(out)
        }
    }
}

impl Drop for FfEncoder {
    fn drop(&mut self) {
        unsafe {
            match &mut self.input {
                InputPath::Sws { sws, frame } => {
                    ff::sws_freeContext(*sws);
                    ff::av_frame_free(frame);
                }
                InputPath::Upload {
                    sws,
                    sw_frame,
                    frames,
                } => {
                    ff::sws_freeContext(*sws);
                    ff::av_frame_free(sw_frame);
                    ff::av_buffer_unref(frames);
                }
                #[cfg(windows)]
                InputPath::D3D11 { frames } => ff::av_buffer_unref(frames),
            }
            ff::avcodec_free_context(&mut self.ctx);
            ff::av_packet_free(&mut self.pkt);
            ff::av_buffer_unref(&mut self.device);
        }
    }
}

#[cfg(windows)]
unsafe fn com_add_ref(object: *mut c_void) {
    // IUnknown vtable: QueryInterface, AddRef, Release.
    unsafe {
        let vtable = *(object as *const *const unsafe extern "system" fn(*mut c_void) -> u32);
        let add_ref = *vtable.add(1);
        add_ref(object);
    }
}

unsafe fn alloc_frame(format: ff::AVPixelFormat, w: c_int, h: c_int) -> Result<*mut ff::AVFrame> {
    unsafe {
        let mut frame = ff::av_frame_alloc();
        if frame.is_null() {
            bail!("out of memory");
        }
        (*frame).format = format as c_int;
        (*frame).width = w;
        (*frame).height = h;
        (*frame).color_primaries = ff::AVColorPrimaries::AVCOL_PRI_BT709;
        (*frame).color_trc = ff::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
        (*frame).colorspace = ff::AVColorSpace::AVCOL_SPC_BT709;
        (*frame).color_range = ff::AVColorRange::AVCOL_RANGE_MPEG;
        let ret = ff::av_frame_get_buffer(frame, 0);
        if ret < 0 {
            ff::av_frame_free(&mut frame);
            check(ret, "av_frame_get_buffer")?;
        }
        Ok(frame)
    }
}

unsafe fn hw_frames(
    device: *mut ff::AVBufferRef,
    format: ff::AVPixelFormat,
    sw_format: ff::AVPixelFormat,
    w: c_int,
    h: c_int,
    pool: c_int,
) -> Result<*mut ff::AVBufferRef> {
    unsafe {
        let mut frames = ff::av_hwframe_ctx_alloc(device);
        if frames.is_null() {
            bail!("av_hwframe_ctx_alloc failed");
        }
        let fc = (*frames).data as *mut ff::AVHWFramesContext;
        (*fc).format = format;
        (*fc).sw_format = sw_format;
        (*fc).width = w;
        (*fc).height = h;
        (*fc).initial_pool_size = pool;
        let ret = ff::av_hwframe_ctx_init(frames);
        if ret < 0 {
            ff::av_buffer_unref(&mut frames);
            check(ret, "av_hwframe_ctx_init")?;
        }
        Ok(frames)
    }
}

/// swscale context converting full-range BGRA to limited-range BT.709 YUV.
unsafe fn bgra_sws(dst: ff::AVPixelFormat, w: c_int, h: c_int) -> Result<*mut ff::SwsContext> {
    unsafe {
        let sws = ff::sws_getContext(
            w,
            h,
            ff::AVPixelFormat::AV_PIX_FMT_BGRA,
            w,
            h,
            dst,
            ff::SwsFlags::SWS_POINT as c_int,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null(),
        );
        if sws.is_null() {
            bail!("sws_getContext failed");
        }
        let coeffs = ff::sws_getCoefficients(ff::SWS_CS_ITU709);
        ff::sws_setColorspaceDetails(sws, coeffs, 1, coeffs, 0, 0, 1 << 16, 1 << 16);
        Ok(sws)
    }
}

unsafe fn convert_bgra(
    sws: *mut ff::SwsContext,
    data: &[u8],
    stride: usize,
    h: c_int,
    dst: *mut ff::AVFrame,
) -> Result<()> {
    if data.len() < stride * h as usize {
        return Err(anyhow!(
            "frame buffer too small ({} < {})",
            data.len(),
            stride * h as usize
        ));
    }
    unsafe {
        let src = [data.as_ptr(), ptr::null(), ptr::null(), ptr::null()];
        let src_stride = [stride as c_int, 0, 0, 0];
        let ret = ff::sws_scale(
            sws,
            src.as_ptr(),
            src_stride.as_ptr(),
            0,
            h,
            (*dst).data.as_ptr(),
            (*dst).linesize.as_ptr(),
        );
        check(ret, "sws_scale")?;
    }
    Ok(())
}

/// A video encoder: FFmpeg for H.264/HEVC/AV1, PyroWave for its codec.
pub struct Encoder(EncoderInner);

enum EncoderInner {
    Ff(FfEncoder),
    Pyro(crate::pyrowave::PyroEncoder),
}

impl Encoder {
    /// Open the best available encoder for `config.codec`.
    pub fn open(config: &EncoderConfig) -> Result<Self> {
        if config.codec == Codec::PyroWave {
            let enc = crate::pyrowave::PyroEncoder::new(
                config.width,
                config.height,
                config.fps,
                config.bitrate_kbps,
            )?;
            tracing::info!(
                width = config.width,
                height = config.height,
                kbps = config.bitrate_kbps,
                "opened PyroWave encoder"
            );
            return Ok(Self(EncoderInner::Pyro(enc)));
        }
        Ok(Self(EncoderInner::Ff(FfEncoder::open(config)?)))
    }

    pub fn name(&self) -> &str {
        match &self.0 {
            EncoderInner::Ff(e) => &e.name,
            EncoderInner::Pyro(_) => "pyrowave",
        }
    }

    /// Whether frames must be passed as [`EncoderInput::Bgra`].
    pub fn wants_cpu_input(&self) -> bool {
        match &self.0 {
            EncoderInner::Ff(e) => e.wants_cpu_input(),
            EncoderInner::Pyro(_) => true,
        }
    }

    /// Encode one frame. Returns `None` if the encoder produced no output
    /// for it (which low-delay encoders should not do).
    pub fn encode(
        &mut self,
        input: EncoderInput<'_>,
        keyframe: bool,
    ) -> Result<Option<EncodedFrame>> {
        match &mut self.0 {
            EncoderInner::Ff(e) => e.encode(input, keyframe),
            EncoderInner::Pyro(e) => match input {
                EncoderInput::Bgra { data, stride } => e.encode_bgra(data, stride).map(Some),
                #[cfg(windows)]
                EncoderInput::D3D11 { .. } => bail!("PyroWave takes CPU frames"),
            },
        }
    }
}

/// Names of encoders built into this FFmpeg that oriel knows how to drive.
pub fn available_encoders() -> Vec<&'static str> {
    [Codec::H264, Codec::Hevc, Codec::Av1]
        .into_iter()
        .flat_map(candidates)
        .filter(|(name, _)| {
            let c = CString::new(*name).unwrap();
            !unsafe { ff::avcodec_find_encoder_by_name(c.as_ptr()) }.is_null()
        })
        .map(|(name, _)| name)
        .chain(crate::pyrowave::available().then_some("pyrowave"))
        .collect()
}
