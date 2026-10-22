use std::{
    ffi::{c_int, c_void},
    ptr,
};

use anyhow::{Result, bail};
use oriel_proto::Codec;

use crate::{
    ff,
    util::{check, is_eagain},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaneFormat {
    /// Y plane + interleaved UV plane at half resolution.
    Nv12,
    /// Y, U, V planes; chroma at half resolution.
    Yuv420p,
    /// Y, U, V planes at full resolution.
    Yuv444p,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorMatrix {
    Bt601,
    Bt709,
    Bt2020,
}

/// A decoded frame in CPU memory.
pub struct DecodedFrame {
    frame: *mut ff::AVFrame,
    pub format: PlaneFormat,
    pub matrix: ColorMatrix,
    pub full_range: bool,
}

unsafe impl Send for DecodedFrame {}

impl DecodedFrame {
    /// Take ownership of a CPU frame in `format`.
    pub(crate) fn from_raw(
        frame: *mut ff::AVFrame,
        format: PlaneFormat,
        matrix: ColorMatrix,
        full_range: bool,
    ) -> Self {
        Self {
            frame,
            format,
            matrix,
            full_range,
        }
    }

    pub fn width(&self) -> u32 {
        unsafe { (*self.frame).width as u32 }
    }

    pub fn height(&self) -> u32 {
        unsafe { (*self.frame).height as u32 }
    }

    pub fn plane_count(&self) -> usize {
        match self.format {
            PlaneFormat::Nv12 => 2,
            PlaneFormat::Yuv420p | PlaneFormat::Yuv444p => 3,
        }
    }

    /// Size of plane `i` in samples (an NV12 UV sample is two bytes).
    pub fn plane_size(&self, i: usize) -> (u32, u32) {
        let (w, h) = (self.width(), self.height());
        match (self.format, i) {
            (_, 0) | (PlaneFormat::Yuv444p, _) => (w, h),
            _ => (w.div_ceil(2), h.div_ceil(2)),
        }
    }

    /// Bytes of plane `i` and its row stride.
    pub fn plane(&self, i: usize) -> (&[u8], usize) {
        let (_, rows) = self.plane_size(i);
        unsafe {
            let stride = (*self.frame).linesize[i] as usize;
            let data = (*self.frame).data[i];
            (
                std::slice::from_raw_parts(data, stride * rows as usize),
                stride,
            )
        }
    }
}

impl Drop for DecodedFrame {
    fn drop(&mut self) {
        unsafe { ff::av_frame_free(&mut self.frame) };
    }
}

/// One plane of a [`DmabufFrame`]. The fd stays owned by the frame.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug)]
pub struct DmabufPlane {
    pub fd: std::os::fd::RawFd,
    pub offset: u64,
    pub pitch: u64,
    pub modifier: u64,
}

/// An 8-bit NV12 hardware frame exported as DMA-BUF planes (VAAPI). While
/// it lives the decoder will not reuse its surface.
#[cfg(target_os = "linux")]
pub struct DmabufFrame {
    /// The DRM PRIME mapping, which references the VAAPI frame.
    frame: *mut ff::AVFrame,
    /// Identifies the decoder surface: the same surface always exports the
    /// same memory, so its imported textures can be reused.
    pub surface: usize,
    pub width: u32,
    pub height: u32,
    /// Luma (R8) and interleaved chroma (GR88).
    pub planes: [DmabufPlane; 2],
    pub matrix: ColorMatrix,
    pub full_range: bool,
}

#[cfg(target_os = "linux")]
unsafe impl Send for DmabufFrame {}

#[cfg(target_os = "linux")]
impl Drop for DmabufFrame {
    fn drop(&mut self) {
        unsafe { ff::av_frame_free(&mut self.frame) };
    }
}

/// A decoded frame: in CPU memory, or left on the GPU as DMA-BUFs.
pub enum Frame {
    Cpu(DecodedFrame),
    #[cfg(target_os = "linux")]
    Dmabuf(DmabufFrame),
}

struct FfDecoder {
    ctx: *mut ff::AVCodecContext,
    pkt: *mut ff::AVPacket,
    frame: *mut ff::AVFrame,
    device: *mut ff::AVBufferRef,
    hw_format: Option<ff::AVPixelFormat>,
    /// Converts formats the renderer does not handle to YUV420P.
    sws: *mut ff::SwsContext,
    description: String,
    /// Hand out hardware frames as DMA-BUFs instead of downloading them.
    export_dmabuf: bool,
}

unsafe impl Send for FfDecoder {}

unsafe extern "C" fn pick_hw_format(
    ctx: *mut ff::AVCodecContext,
    formats: *const ff::AVPixelFormat,
) -> ff::AVPixelFormat {
    unsafe {
        let wanted = (*ctx).opaque as usize as c_int;
        let mut p = formats;
        while *p != ff::AVPixelFormat::AV_PIX_FMT_NONE {
            if *p as c_int == wanted {
                return *p;
            }
            p = p.add(1);
        }
        // Hardware unavailable for this stream: take the first software
        // format FFmpeg offers.
        tracing::warn!("hardware decoding not available for this stream, using software");
        *formats
    }
}

fn hw_device_types() -> &'static [ff::AVHWDeviceType] {
    if cfg!(windows) {
        &[ff::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA]
    } else if cfg!(target_os = "macos") {
        &[ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VIDEOTOOLBOX]
    } else {
        &[ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI]
    }
}

impl FfDecoder {
    fn open(codec: Codec, hardware: bool, export_dmabuf: bool) -> Result<Self> {
        let id = match codec {
            Codec::H264 => ff::AVCodecID::AV_CODEC_ID_H264,
            Codec::Hevc => ff::AVCodecID::AV_CODEC_ID_HEVC,
            Codec::Av1 => ff::AVCodecID::AV_CODEC_ID_AV1,
            Codec::PyroWave => bail!("PyroWave is not decoded by FFmpeg"),
        };
        unsafe {
            let decoder = ff::avcodec_find_decoder(id);
            if decoder.is_null() {
                bail!("no {} decoder in this FFmpeg build", codec.name());
            }
            let mut dec = FfDecoder {
                ctx: ff::avcodec_alloc_context3(decoder),
                pkt: ff::av_packet_alloc(),
                frame: ff::av_frame_alloc(),
                device: ptr::null_mut(),
                hw_format: None,
                sws: ptr::null_mut(),
                description: String::new(),
                export_dmabuf: export_dmabuf && cfg!(target_os = "linux"),
            };
            let ctx = &mut *dec.ctx;
            ctx.flags |= ff::AV_CODEC_FLAG_LOW_DELAY as c_int;
            ctx.flags2 |= ff::AV_CODEC_FLAG2_FAST;
            // Frame threading adds a frame of latency per thread.
            ctx.thread_type = ff::FF_THREAD_SLICE;
            ctx.thread_count = 0;

            if hardware {
                dec.try_hardware(decoder);
            }
            if dec.export_dmabuf {
                // Exported frames stay referenced by the renderer for a few
                // frames; keep enough surfaces for the decoder meanwhile.
                ctx.extra_hw_frames = 6;
            }
            check(
                ff::avcodec_open2(dec.ctx, decoder, ptr::null_mut()),
                "open decoder",
            )?;
            let name = std::ffi::CStr::from_ptr((*decoder).name).to_string_lossy();
            dec.description = match dec.hw_format {
                Some(f) => format!("{name} ({})", hw_name(f)),
                None => format!("{name} (software)"),
            };
            tracing::info!(decoder = %dec.description, "opened video decoder");
            Ok(dec)
        }
    }

    unsafe fn try_hardware(&mut self, decoder: *const ff::AVCodec) {
        for &ty in hw_device_types() {
            unsafe {
                let mut i = 0;
                let format = loop {
                    let config = ff::avcodec_get_hw_config(decoder, i);
                    if config.is_null() {
                        break None;
                    }
                    if (*config).methods & ff::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX as c_int != 0
                        && (*config).device_type == ty
                    {
                        break Some((*config).pix_fmt);
                    }
                    i += 1;
                };
                let Some(format) = format else { continue };
                let mut device = ptr::null_mut();
                let ret =
                    ff::av_hwdevice_ctx_create(&mut device, ty, ptr::null(), ptr::null_mut(), 0);
                if ret < 0 {
                    tracing::info!(
                        "hardware decoder device unavailable: {}",
                        crate::util::err_str(ret)
                    );
                    continue;
                }
                self.device = device;
                self.hw_format = Some(format);
                (*self.ctx).hw_device_ctx = ff::av_buffer_ref(device);
                (*self.ctx).opaque = format as c_int as usize as *mut c_void;
                (*self.ctx).get_format = Some(pick_hw_format);
                return;
            }
        }
    }

    /// Decode one access unit. Returns the newest frame it produced.
    fn decode(&mut self, data: &[u8]) -> Result<Option<Frame>> {
        unsafe {
            check(
                ff::av_new_packet(self.pkt, data.len() as c_int),
                "av_new_packet",
            )?;
            ptr::copy_nonoverlapping(data.as_ptr(), (*self.pkt).data, data.len());
            let ret = ff::avcodec_send_packet(self.ctx, self.pkt);
            ff::av_packet_unref(self.pkt);
            check(ret, "avcodec_send_packet")?;

            let mut latest = None;
            loop {
                let ret = ff::avcodec_receive_frame(self.ctx, self.frame);
                if is_eagain(ret) || ret == crate::util::AVERROR_EOF {
                    break;
                }
                check(ret, "avcodec_receive_frame")?;
                latest = Some(self.take_frame()?);
            }
            Ok(latest)
        }
    }

    unsafe fn take_frame(&mut self) -> Result<Frame> {
        unsafe {
            let mut out = ff::av_frame_alloc();
            if Some((*self.frame).format) == self.hw_format.map(|f| f as c_int) {
                #[cfg(target_os = "linux")]
                if self.export_dmabuf {
                    match self.map_dmabuf() {
                        Ok(frame) => {
                            ff::av_frame_free(&mut out);
                            return Ok(Frame::Dmabuf(frame));
                        }
                        Err(e) => {
                            tracing::info!("DMA-BUF export unavailable, downloading frames: {e:#}");
                            self.export_dmabuf = false;
                        }
                    }
                }
                let ret = ff::av_hwframe_transfer_data(out, self.frame, 0);
                if ret < 0 {
                    ff::av_frame_free(&mut out);
                    ff::av_frame_unref(self.frame);
                    check(ret, "download hardware frame")?;
                }
                ff::av_frame_copy_props(out, self.frame);
                ff::av_frame_unref(self.frame);
            } else {
                ff::av_frame_move_ref(out, self.frame);
            }

            let fmt = (*out).format;
            let mut full_range = (*out).color_range == ff::AVColorRange::AVCOL_RANGE_JPEG;
            use ff::AVPixelFormat::*;
            let format = if fmt == AV_PIX_FMT_NV12 as c_int {
                PlaneFormat::Nv12
            } else if fmt == AV_PIX_FMT_YUV420P as c_int {
                PlaneFormat::Yuv420p
            } else if fmt == AV_PIX_FMT_YUVJ420P as c_int {
                full_range = true;
                PlaneFormat::Yuv420p
            } else if fmt == AV_PIX_FMT_YUV444P as c_int {
                PlaneFormat::Yuv444p
            } else {
                out = self.convert_to_yuv420p(out)?;
                PlaneFormat::Yuv420p
            };
            let matrix = color_matrix(out);
            Ok(Frame::Cpu(DecodedFrame {
                frame: out,
                format,
                matrix,
                full_range,
            }))
        }
    }

    /// Map the current VAAPI frame to DMA-BUF planes. Mapping for reading
    /// waits for the decode to finish. On error the frame is left as is.
    #[cfg(target_os = "linux")]
    unsafe fn map_dmabuf(&mut self) -> Result<DmabufFrame> {
        // DRM fourccs of the layers VAAPI exports NV12 as.
        const R8: u32 = u32::from_le_bytes(*b"R8  ");
        const GR88: u32 = u32::from_le_bytes(*b"GR88");
        const NV12: u32 = u32::from_le_bytes(*b"NV12");
        unsafe {
            let src = self.frame;
            let mut map = ff::av_frame_alloc();
            (*map).format = ff::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as c_int;
            let ret = ff::av_hwframe_map(map, src, ff::AV_HWFRAME_MAP_READ as c_int);
            if ret < 0 {
                ff::av_frame_free(&mut map);
                check(ret, "map frame to DRM PRIME")?;
            }
            let desc = &*((*map).data[0] as *const ff::AVDRMFrameDescriptor);
            let layers = &desc.layers[..desc.nb_layers.clamp(0, 4) as usize];
            let formats: Vec<u32> = layers.iter().map(|l| l.format).collect();
            let planes: Vec<_> = layers
                .iter()
                .flat_map(|l| &l.planes[..l.nb_planes.clamp(0, 4) as usize])
                .map(|p| {
                    let object = &desc.objects[p.object_index.clamp(0, 3) as usize];
                    DmabufPlane {
                        fd: object.fd,
                        offset: p.offset as u64,
                        pitch: p.pitch as u64,
                        modifier: object.format_modifier,
                    }
                })
                .collect();
            if !(formats == [R8, GR88] || formats == [NV12]) || planes.len() != 2 {
                ff::av_frame_free(&mut map);
                bail!("unsupported DRM layout {formats:08x?}");
            }
            let frame = DmabufFrame {
                frame: map,
                surface: (*src).data[3] as usize,
                width: (*src).width as u32,
                height: (*src).height as u32,
                planes: [planes[0], planes[1]],
                matrix: color_matrix(src),
                full_range: (*src).color_range == ff::AVColorRange::AVCOL_RANGE_JPEG,
            };
            ff::av_frame_unref(src);
            Ok(frame)
        }
    }

    unsafe fn convert_to_yuv420p(&mut self, src: *mut ff::AVFrame) -> Result<*mut ff::AVFrame> {
        unsafe {
            let (w, h) = ((*src).width, (*src).height);
            self.sws = ff::sws_getCachedContext(
                self.sws,
                w,
                h,
                std::mem::transmute::<c_int, ff::AVPixelFormat>((*src).format),
                w,
                h,
                ff::AVPixelFormat::AV_PIX_FMT_YUV420P,
                ff::SwsFlags::SWS_POINT as c_int,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
            );
            let mut src = src;
            if self.sws.is_null() {
                ff::av_frame_free(&mut src);
                bail!("unsupported decoded pixel format {}", (*src).format);
            }
            let mut dst = ff::av_frame_alloc();
            (*dst).format = ff::AVPixelFormat::AV_PIX_FMT_YUV420P as c_int;
            (*dst).width = w;
            (*dst).height = h;
            let ret = ff::av_frame_get_buffer(dst, 0);
            if ret >= 0 {
                ff::sws_scale(
                    self.sws,
                    (*src).data.as_ptr() as *const *const u8,
                    (*src).linesize.as_ptr(),
                    0,
                    h,
                    (*dst).data.as_ptr(),
                    (*dst).linesize.as_ptr(),
                );
                ff::av_frame_copy_props(dst, src);
            }
            ff::av_frame_free(&mut src);
            if ret < 0 {
                ff::av_frame_free(&mut dst);
                check(ret, "av_frame_get_buffer")?;
            }
            Ok(dst)
        }
    }
}

/// A video decoder: FFmpeg (preferring hardware) or PyroWave.
pub struct Decoder {
    inner: DecoderInner,
    description: String,
}

enum DecoderInner {
    Ff(FfDecoder),
    Pyro(crate::pyrowave::PyroDecoder),
}

impl Decoder {
    pub fn open(codec: Codec, width: u32, height: u32, hardware: bool) -> Result<Self> {
        Self::open_with(codec, width, height, hardware, false)
    }

    /// Like [`Decoder::open`]; with `export_dmabuf`, hardware frames are
    /// returned as [`Frame::Dmabuf`] where possible (Linux VAAPI).
    pub fn open_with(
        codec: Codec,
        width: u32,
        height: u32,
        hardware: bool,
        export_dmabuf: bool,
    ) -> Result<Self> {
        if codec == Codec::PyroWave {
            let dec = crate::pyrowave::PyroDecoder::new(width, height)?;
            return Ok(Self {
                inner: DecoderInner::Pyro(dec),
                description: "pyrowave (Vulkan)".into(),
            });
        }
        let dec = FfDecoder::open(codec, hardware, export_dmabuf)?;
        Ok(Self {
            description: dec.description.clone(),
            inner: DecoderInner::Ff(dec),
        })
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    /// Decode one frame's data. Returns the newest frame it produced.
    pub fn decode(&mut self, data: &[u8]) -> Result<Option<DecodedFrame>> {
        match self.decode_frame(data)? {
            None => Ok(None),
            Some(Frame::Cpu(f)) => Ok(Some(f)),
            #[cfg(target_os = "linux")]
            Some(Frame::Dmabuf(_)) => bail!("decoder opened for DMA-BUF export"),
        }
    }

    /// Decode one frame's data, keeping hardware frames on the GPU when the
    /// decoder was opened for that.
    pub fn decode_frame(&mut self, data: &[u8]) -> Result<Option<Frame>> {
        match &mut self.inner {
            DecoderInner::Ff(d) => d.decode(data),
            DecoderInner::Pyro(d) => Ok(d.decode(data)?.map(Frame::Cpu)),
        }
    }
}

fn hw_name(format: ff::AVPixelFormat) -> String {
    unsafe {
        let name = ff::av_get_pix_fmt_name(format);
        if name.is_null() {
            "hardware".into()
        } else {
            std::ffi::CStr::from_ptr(name)
                .to_string_lossy()
                .into_owned()
        }
    }
}

unsafe fn color_matrix(frame: *const ff::AVFrame) -> ColorMatrix {
    unsafe {
        match (*frame).colorspace {
            ff::AVColorSpace::AVCOL_SPC_BT709 => ColorMatrix::Bt709,
            ff::AVColorSpace::AVCOL_SPC_BT2020_NCL | ff::AVColorSpace::AVCOL_SPC_BT2020_CL => {
                ColorMatrix::Bt2020
            }
            ff::AVColorSpace::AVCOL_SPC_UNSPECIFIED if (*frame).height > 576 => ColorMatrix::Bt709,
            _ => ColorMatrix::Bt601,
        }
    }
}

impl Drop for FfDecoder {
    fn drop(&mut self) {
        unsafe {
            ff::avcodec_free_context(&mut self.ctx);
            ff::av_packet_free(&mut self.pkt);
            ff::av_frame_free(&mut self.frame);
            ff::av_buffer_unref(&mut self.device);
            ff::sws_freeContext(self.sws);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EncoderConfig, EncoderInput, HwDevice, encoder::Encoder};

    /// Software round trip; needs an FFmpeg with libx264.
    #[test]
    fn h264_software_roundtrip() {
        roundtrip(Codec::H264, "libx264", false, 320, 240);
    }

    /// Hardware round trip on a VAAPI GPU; skipped when none is present.
    #[cfg(target_os = "linux")]
    #[test]
    fn hevc_vaapi_roundtrip() {
        roundtrip(Codec::Hevc, "hevc_vaapi", true, 1280, 720);
    }

    /// VAAPI frames export as two DMA-BUF planes; skipped without a GPU.
    #[cfg(target_os = "linux")]
    #[test]
    fn hevc_vaapi_dmabuf_export() {
        let (w, h) = (1280, 720);
        let config = EncoderConfig {
            codec: Codec::Hevc,
            width: w,
            height: h,
            fps: 60,
            bitrate_kbps: 4000,
            device: HwDevice::None,
            encoder: Some("hevc_vaapi".into()),
        };
        let mut enc = match Encoder::open(&config) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("skipping: {e:#}");
                return;
            }
        };
        let mut dec = Decoder::open_with(Codec::Hevc, w, h, true, true).unwrap();
        let bgra = vec![128u8; (w * h * 4) as usize];
        let mut held = Vec::new();
        for n in 0..8 {
            let input = EncoderInput::Bgra {
                data: &bgra,
                stride: (w * 4) as usize,
            };
            let out = enc.encode(input, n == 0).unwrap().unwrap();
            match dec.decode_frame(&out.data).unwrap().unwrap() {
                Frame::Dmabuf(f) => {
                    assert_eq!((f.width, f.height), (w, h));
                    assert!(f.planes.iter().all(|p| p.fd >= 0 && p.pitch >= w as u64));
                    eprintln!("{:?}", f.planes);
                    // Hold a few frames, as the renderer does.
                    held.push(f);
                    if held.len() > 4 {
                        held.remove(0);
                    }
                }
                Frame::Cpu(_) => panic!("expected a DMA-BUF frame"),
            }
        }
    }

    /// Needs the PyroWave library and a Vulkan 1.3 GPU; skipped otherwise.
    #[test]
    fn pyrowave_roundtrip() {
        if !crate::pyrowave::available() {
            eprintln!("skipping: PyroWave library not found");
            return;
        }
        roundtrip(Codec::PyroWave, "pyrowave", true, 1280, 720);
    }

    fn roundtrip(codec: Codec, encoder: &str, hardware: bool, w: u32, h: u32) {
        let config = EncoderConfig {
            codec,
            width: w,
            height: h,
            fps: 60,
            bitrate_kbps: 4000,
            device: HwDevice::None,
            encoder: Some(encoder.into()),
        };
        let mut enc = match Encoder::open(&config) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("skipping: {e:#}");
                return;
            }
        };
        let mut dec = Decoder::open(codec, w, h, hardware).unwrap();
        let mut bgra = vec![0u8; (w * h * 4) as usize];
        for (i, px) in bgra.chunks_mut(4).enumerate() {
            px.copy_from_slice(&[(i % 255) as u8, 128, 64, 255]);
        }
        let mut decoded = 0;
        for n in 0..5 {
            let out = enc
                .encode(
                    EncoderInput::Bgra {
                        data: &bgra,
                        stride: (w * 4) as usize,
                    },
                    n == 0,
                )
                .unwrap()
                .expect("low-delay encoder returns a packet per frame");
            assert_eq!(out.keyframe, n == 0 || codec == Codec::PyroWave);
            if let Some(frame) = dec.decode(&out.data).unwrap() {
                assert_eq!((frame.width(), frame.height()), (w, h));
                assert_eq!(frame.matrix, ColorMatrix::Bt709);
                assert_eq!(frame.full_range, codec == Codec::PyroWave);
                // The encoded colour (B=i, G=128, R=64) survives the trip.
                let (y, _) = frame.plane(0);
                let luma = y[0] as f32;
                assert!((40.0..140.0).contains(&luma), "luma {luma}");
                decoded += 1;
            }
        }
        assert_eq!(decoded, 5, "decoder must not buffer frames");
    }
}

#[cfg(all(test, target_os = "linux"))]
mod refine_tests {
    use super::*;
    use crate::{EncoderConfig, EncoderInput, HwDevice, encoder::Encoder};

    /// Text-like page: rows of glyphs drawn from a small random font.
    fn page(w: usize, h: usize, scroll: usize) -> Vec<u8> {
        let mut font = [[0u8; 54]; 64];
        let mut x = 12345u32;
        for g in font.iter_mut() {
            for b in g.iter_mut() {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                *b = x.is_multiple_of(3) as u8;
            }
        }
        let mut bgra = vec![255u8; w * h * 4];
        for row in 0..h {
            let y = row + scroll;
            let (line, gy) = (y / 14, y % 14);
            for col in 0..w {
                let (cell, gx) = (col / 8, col % 8);
                if gy < 9 && gx < 6 {
                    let glyph = (line * 131 + cell * 17) % 64;
                    if font[glyph][gy * 6 + gx] == 1 {
                        bgra[(row * w + col) * 4..][..3].copy_from_slice(&[30, 30, 30]);
                    }
                }
            }
        }
        bgra
    }

    fn psnr(frame: &DecodedFrame, bgra: &[u8], w: usize, h: usize) -> f64 {
        let (y, stride) = frame.plane(0);
        let mut se = 0f64;
        for row in 0..h {
            for col in 0..w {
                let expect = 16.0 + bgra[(row * w + col) * 4] as f64 * 219.0 / 255.0;
                let d = y[row * stride + col] as f64 - expect;
                se += d * d;
            }
        }
        10.0 * (255.0f64 * 255.0 / (se / (w * h) as f64)).log10()
    }

    /// Re-encoding an unchanged image should sharpen it (the host sends a
    /// few such frames once the screen stops changing).
    #[test]
    fn repeated_frames_refine_quality() {
        {
            let (encoder, codec) = ("hevc_vaapi", Codec::Hevc);
            let (w, h) = (1280usize, 720usize);
            let config = EncoderConfig {
                codec,
                width: w as u32,
                height: h as u32,
                fps: 120,
                bitrate_kbps: 12_000,
                device: HwDevice::None,
                encoder: Some(encoder.into()),
            };
            let Ok(mut enc) = Encoder::open(&config) else {
                eprintln!("skipping: no {encoder}");
                return;
            };
            let mut dec = Decoder::open(codec, w as u32, h as u32, false).unwrap();
            let mut results = Vec::new();
            // Scroll for 20 frames, then stop for as many refresh frames as
            // the host sends.
            for n in 0..20 + 30usize {
                let bgra = page(w, h, 40 * n.min(20));
                let input = EncoderInput::Bgra {
                    data: &bgra,
                    stride: w * 4,
                };
                let out = enc.encode(input, n == 0).unwrap().unwrap();
                let frame = dec.decode(&out.data).unwrap().unwrap();
                if n >= 20 && (n - 20) % 5 == 0 {
                    results.push((psnr(&frame, &bgra, w, h), out.data.len() / 1024));
                }
            }
            eprintln!("{encoder} (last moving frame, then static): {results:.1?}");
            let (first, last) = (results[0].0, results.last().unwrap().0);
            assert!(last > first + 2.0, "{first:.1} dB → {last:.1} dB");
        }
    }
}
