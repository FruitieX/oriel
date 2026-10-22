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

pub struct Decoder {
    ctx: *mut ff::AVCodecContext,
    pkt: *mut ff::AVPacket,
    frame: *mut ff::AVFrame,
    device: *mut ff::AVBufferRef,
    hw_format: Option<ff::AVPixelFormat>,
    /// Converts formats the renderer does not handle to YUV420P.
    sws: *mut ff::SwsContext,
    description: String,
}

unsafe impl Send for Decoder {}

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

impl Decoder {
    pub fn open(codec: Codec, hardware: bool) -> Result<Self> {
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
            let mut dec = Decoder {
                ctx: ff::avcodec_alloc_context3(decoder),
                pkt: ff::av_packet_alloc(),
                frame: ff::av_frame_alloc(),
                device: ptr::null_mut(),
                hw_format: None,
                sws: ptr::null_mut(),
                description: String::new(),
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

    pub fn description(&self) -> &str {
        &self.description
    }

    /// Decode one access unit. Returns the newest frame it produced.
    pub fn decode(&mut self, data: &[u8]) -> Result<Option<DecodedFrame>> {
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

    unsafe fn take_frame(&mut self) -> Result<DecodedFrame> {
        unsafe {
            let mut out = ff::av_frame_alloc();
            if Some((*self.frame).format) == self.hw_format.map(|f| f as c_int) {
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
            let matrix = match (*out).colorspace {
                ff::AVColorSpace::AVCOL_SPC_BT709 => ColorMatrix::Bt709,
                ff::AVColorSpace::AVCOL_SPC_BT2020_NCL | ff::AVColorSpace::AVCOL_SPC_BT2020_CL => {
                    ColorMatrix::Bt2020
                }
                ff::AVColorSpace::AVCOL_SPC_UNSPECIFIED if (*out).height > 576 => {
                    ColorMatrix::Bt709
                }
                _ => ColorMatrix::Bt601,
            };
            Ok(DecodedFrame {
                frame: out,
                format,
                matrix,
                full_range,
            })
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
                    (*dst).data.as_ptr() as *const *mut u8,
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

impl Drop for Decoder {
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
        let mut dec = Decoder::open(codec, hardware).unwrap();
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
            assert_eq!(out.keyframe, n == 0);
            if let Some(frame) = dec.decode(&out.data).unwrap() {
                assert_eq!((frame.width(), frame.height()), (w, h));
                assert_eq!(frame.matrix, ColorMatrix::Bt709);
                assert!(!frame.full_range);
                decoded += 1;
            }
        }
        assert_eq!(decoded, 5, "decoder must not buffer frames");
    }
}
