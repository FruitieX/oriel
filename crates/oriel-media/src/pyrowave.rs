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
    // GPU interop (used by the Windows zero-copy path).
    #[allow(dead_code)]
    create_device_by_compat: unsafe extern "C" fn(
        u32,
        u32,
        *const [u8; 16],
        *const [u8; 16],
        *const [u8; 8],
        i32,
        *mut Handle,
    ) -> c_int,
    #[allow(dead_code)]
    image_create: unsafe extern "C" fn(*const gpu::ImageCreateInfo, *mut Handle) -> c_int,
    #[allow(dead_code)]
    image_destroy: unsafe extern "C" fn(Handle),
    #[allow(dead_code)]
    image_get_image_view: unsafe extern "C" fn(Handle, u32, u32, *mut gpu::ImageView) -> c_int,
    #[allow(dead_code)]
    sync_object_create: unsafe extern "C" fn(*const gpu::SyncCreateInfo, *mut Handle) -> c_int,
    #[allow(dead_code)]
    sync_object_get_semaphore: unsafe extern "C" fn(Handle) -> u64,
    #[allow(dead_code)]
    sync_object_destroy: unsafe extern "C" fn(Handle),
    #[allow(dead_code)]
    encoder_encode_gpu_scaled: unsafe extern "C" fn(
        Handle,
        *const gpu::SyncOp,
        *const gpu::SyncOp,
        *const gpu::ScaledEncodeInfo,
        *const RateControl,
    ) -> c_int,
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
            create_device_by_compat: sym!("pyrowave_create_device_by_compat"),
            image_create: sym!("pyrowave_image_create"),
            image_destroy: sym!("pyrowave_image_destroy"),
            image_get_image_view: sym!("pyrowave_image_get_image_view"),
            sync_object_create: sym!("pyrowave_sync_object_create"),
            sync_object_get_semaphore: sym!("pyrowave_sync_object_get_semaphore"),
            sync_object_destroy: sym!("pyrowave_sync_object_destroy"),
            encoder_encode_gpu_scaled: sym!("pyrowave_encoder_encode_gpu_scaled"),
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

impl Device {
    /// A device on the GPU with this adapter LUID (for sharing textures).
    #[cfg_attr(not(windows), allow(dead_code))]
    fn on_adapter(luid: [u8; 8]) -> Result<Self> {
        let api = api()?;
        let mut device = ptr::null_mut();
        // HIGH priority puts encoding on an async compute queue, so it does
        // not wait behind a game's rendering.
        for priority in [gpu::QUEUE_GLOBAL_PRIORITY_HIGH, gpu::QUEUE_GLOBAL_PRIORITY_MEDIUM] {
            let r = unsafe {
                (api.create_device_by_compat)(0, 0, ptr::null(), ptr::null(), &luid, priority, &mut device)
            };
            if r == SUCCESS {
                return Ok(Self(device));
            }
        }
        bail!("cannot create a PyroWave device on the capture GPU")
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
    /// Zero-copy input from a shared D3D11 texture.
    #[cfg(windows)]
    gpu: Option<d3d11::GpuInput>,
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
            #[cfg(windows)]
            gpu: None,
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
        }
        self.finish(started)
    }

    /// Packetize the frame just encoded.
    fn finish(&mut self, started: Instant) -> Result<EncodedFrame> {
        let api = api()?;
        unsafe {
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
        #[cfg(windows)]
        drop(self.gpu.take());
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

/// C API types for GPU interop.
mod gpu {
    #![allow(dead_code)]
    use super::Handle;

    pub const QUEUE_GLOBAL_PRIORITY_MEDIUM: i32 = 256;
    pub const QUEUE_GLOBAL_PRIORITY_HIGH: i32 = 512;
    pub const QUEUE_FAMILY_EXTERNAL: u32 = !1;
    pub const ASPECT_COLOR: u32 = 1;
    pub const USAGE_SAMPLED: u32 = 4;
    pub const COLOR_SPACE_SRGB_NONLINEAR: i32 = 0;
    pub const FORMAT_R8_UNORM: i32 = 9;
    pub const FORMAT_B8G8R8A8_UNORM: i32 = 44;
    pub const EXTERNAL_MEMORY_D3D11_TEXTURE: u32 = 0x8;
    pub const EXTERNAL_SEMAPHORE_D3D12_FENCE: u32 = 0x8;
    pub const SEMAPHORE_TYPE_TIMELINE: i32 = 1;

    #[repr(C)]
    pub struct ImageCreateInfo {
        pub device: Handle,
        pub external_handle: usize,
        pub handle_type: u32,
        pub image_create_info: *const ash::vk::ImageCreateInfo<'static>,
    }

    #[repr(C)]
    pub struct SyncCreateInfo {
        pub device: Handle,
        pub external_handle: usize,
        pub handle_type: u32,
        pub semaphore_type: i32,
        pub import_flags: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct ImageView {
        pub image: u64,
        pub width: u32,
        pub height: u32,
        pub image_format: i32,
        pub view_format: i32,
        pub mip_level: u32,
        pub layer: u32,
        pub aspect: u32,
        pub swizzle: i32,
        pub layout: i32,
    }

    #[repr(C)]
    pub struct ExternalRef {
        pub image: Handle,
        pub queue_family_index: u32,
    }

    #[repr(C)]
    pub struct SyncPoint {
        pub semaphore: u64,
        pub value: u64,
    }

    #[repr(C)]
    pub struct SyncOp {
        pub images: *const ExternalRef,
        pub num_images: usize,
        pub sync: SyncPoint,
    }

    #[repr(C)]
    pub struct ScaledEncodeInfo {
        pub view: ImageView,
        pub input_color_space: i32,
        pub output_color_space: i32,
        pub intermediate_plane_format: i32,
        pub ycbcr_chroma_midpoint: f32,
        pub force_linear_filtering: bool,
        pub skip_dither: bool,
        pub crop_rect: *const ash::vk::Rect2D,
        pub ycbcr_range: i32,
        pub ycbcr_range_bit_depth: u32,
    }
}

/// Windows zero-copy input: the captured frame is copied (on the GPU) into
/// a texture shared with PyroWave's Vulkan device, which converts and
/// encodes it without touching the CPU.
#[cfg(windows)]
mod d3d11 {
    use std::ffi::c_void;

    use anyhow::{Context, Result};
    use windows::{
        Win32::{
            Foundation::GENERIC_ALL,
            Graphics::{
                Direct3D11::{
                    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_FENCE_FLAG_SHARED,
                    D3D11_RESOURCE_MISC_SHARED, D3D11_RESOURCE_MISC_SHARED_NTHANDLE,
                    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, ID3D11Device, ID3D11Device5,
                    ID3D11DeviceContext4, ID3D11Fence, ID3D11Texture2D,
                },
                Dxgi::{
                    Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC},
                    IDXGIDevice, IDXGIResource1,
                },
            },
        },
        core::Interface,
    };

    use super::{Handle, api, gpu, pw};

    pub struct GpuInput {
        pub texture: ID3D11Texture2D,
        context: ID3D11DeviceContext4,
        fence: ID3D11Fence,
        fence_value: u64,
        image: Handle,
        sync: Handle,
    }

    // D3D11 devices with multithread protection and PyroWave objects are
    // used from the pipeline thread only.
    unsafe impl Send for GpuInput {}

    /// The adapter LUID of a D3D11 device.
    pub fn adapter_luid(device: &ID3D11Device) -> Result<[u8; 8]> {
        let desc = unsafe { device.cast::<IDXGIDevice>()?.GetAdapter()?.GetDesc()? };
        let luid = desc.AdapterLuid;
        let mut out = [0u8; 8];
        out[..4].copy_from_slice(&luid.LowPart.to_le_bytes());
        out[4..].copy_from_slice(&luid.HighPart.to_le_bytes());
        Ok(out)
    }

    impl GpuInput {
        pub fn new(device: &ID3D11Device, pyro_device: Handle, (w, h): (u32, u32)) -> Result<Self> {
            let api = api()?;
            unsafe {
                let device5: ID3D11Device5 = device.cast()?;
                let context: ID3D11DeviceContext4 = device.GetImmediateContext()?.cast()?;

                let desc = D3D11_TEXTURE2D_DESC {
                    Width: w,
                    Height: h,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: (D3D11_RESOURCE_MISC_SHARED.0 | D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0) as u32,
                };
                let mut texture = None;
                device.CreateTexture2D(&desc, None, Some(&mut texture))?;
                let texture = texture.context("CreateTexture2D returned nothing")?;
                let handle = texture
                    .cast::<IDXGIResource1>()?
                    .CreateSharedHandle(None, GENERIC_ALL.0, None)?;

                let image_info = ash::vk::ImageCreateInfo::default()
                    .image_type(ash::vk::ImageType::TYPE_2D)
                    .format(ash::vk::Format::B8G8R8A8_UNORM)
                    .extent(ash::vk::Extent3D { width: w, height: h, depth: 1 })
                    .mip_levels(1)
                    .array_layers(1)
                    .samples(ash::vk::SampleCountFlags::TYPE_1)
                    .tiling(ash::vk::ImageTiling::OPTIMAL)
                    .usage(ash::vk::ImageUsageFlags::SAMPLED | ash::vk::ImageUsageFlags::TRANSFER_SRC)
                    .sharing_mode(ash::vk::SharingMode::EXCLUSIVE);
                let info = gpu::ImageCreateInfo {
                    device: pyro_device,
                    // PyroWave takes ownership of the handle.
                    external_handle: handle.0 as usize,
                    handle_type: gpu::EXTERNAL_MEMORY_D3D11_TEXTURE,
                    image_create_info: &image_info as *const _ as *const _,
                };
                let mut image = std::ptr::null_mut();
                pw((api.image_create)(&info, &mut image), "import the shared texture into Vulkan")?;

                let fence: ID3D11Fence = {
                    let mut f: Option<ID3D11Fence> = None;
                    device5.CreateFence(0, D3D11_FENCE_FLAG_SHARED, &mut f)?;
                    f.context("CreateFence returned nothing")?
                };
                let fence_handle = fence.CreateSharedHandle(None, GENERIC_ALL.0, None)?;
                let sync_info = gpu::SyncCreateInfo {
                    device: pyro_device,
                    external_handle: fence_handle.0 as usize,
                    handle_type: gpu::EXTERNAL_SEMAPHORE_D3D12_FENCE,
                    semaphore_type: gpu::SEMAPHORE_TYPE_TIMELINE,
                    import_flags: 0,
                };
                let mut sync = std::ptr::null_mut();
                let r = (api.sync_object_create)(&sync_info, &mut sync);
                if let Err(e) = pw(r, "import the D3D11 fence into Vulkan") {
                    (api.image_destroy)(image);
                    return Err(e);
                }
                Ok(Self {
                    texture,
                    context,
                    fence,
                    fence_value: 0,
                    image,
                    sync,

                })
            }
        }

        /// The texture is filled: have Vulkan wait for the copy on the GPU
        /// and encode.
        pub fn encode(&mut self, encoder: Handle, rate: &super::RateControl) -> Result<()> {
            let api = api()?;
            unsafe {
                self.fence_value += 1;
                self.context.Signal(&self.fence, self.fence_value)?;
                self.context.Flush();
                let refs = [gpu::ExternalRef {
                    image: self.image,
                    queue_family_index: gpu::QUEUE_FAMILY_EXTERNAL,
                }];
                let acquire = gpu::SyncOp {
                    images: refs.as_ptr(),
                    num_images: 1,
                    sync: gpu::SyncPoint {
                        semaphore: (api.sync_object_get_semaphore)(self.sync),
                        value: self.fence_value,
                    },
                };
                // Released when the encoder is done; packetizing waits for
                // that before the next frame is copied in.
                let release = gpu::SyncOp {
                    images: refs.as_ptr(),
                    num_images: 1,
                    sync: gpu::SyncPoint { semaphore: 0, value: 0 },
                };
                let mut view = gpu::ImageView::default();
                pw(
                    (api.image_get_image_view)(self.image, gpu::ASPECT_COLOR, gpu::USAGE_SAMPLED, &mut view),
                    "PyroWave image view",
                )?;
                let info = gpu::ScaledEncodeInfo {
                    view,
                    input_color_space: gpu::COLOR_SPACE_SRGB_NONLINEAR,
                    output_color_space: gpu::COLOR_SPACE_SRGB_NONLINEAR,
                    intermediate_plane_format: gpu::FORMAT_R8_UNORM,
                    ycbcr_chroma_midpoint: 0.5,
                    // Same size in and out: no filtering needed.
                    force_linear_filtering: true,
                    skip_dither: false,
                    crop_rect: std::ptr::null(),
                    ycbcr_range: 0, // full range, BT.709
                    ycbcr_range_bit_depth: 0,
                };
                pw(
                    (api.encoder_encode_gpu_scaled)(encoder, &acquire, &release, &info, rate),
                    "PyroWave GPU encode",
                )
            }
        }

        pub fn texture_ptr(&self) -> *mut c_void {
            self.texture.as_raw()
        }
    }

    impl Drop for GpuInput {
        fn drop(&mut self) {
            if let Ok(api) = api() {
                unsafe {
                    (api.sync_object_destroy)(self.sync);
                    (api.image_destroy)(self.image);
                }
            }
        }
    }
}

#[cfg(windows)]
impl PyroEncoder {
    /// Zero-copy encoder for frames on `device` (an `ID3D11Device*`).
    pub fn new_d3d11(
        device: *mut c_void,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_kbps: u32,
    ) -> Result<Self> {
        use windows::{Win32::Graphics::Direct3D11::ID3D11Device, core::Interface};
        let d3d = unsafe { ID3D11Device::from_raw_borrowed(&device) }
            .ok_or_else(|| anyhow!("null D3D11 device"))?;
        let luid = d3d11::adapter_luid(d3d)?;
        let api = api()?;
        let device = Device::on_adapter(luid)?;
        let gpu = d3d11::GpuInput::new(d3d, device.0, (width, height))?;
        let mut encoder = ptr::null_mut();
        let info = CreateInfo {
            device: device.0,
            width: width as c_int,
            height: height as c_int,
            chroma: CHROMA_420,
        };
        pw(unsafe { (api.encoder_create)(&info, &mut encoder) }, "create PyroWave encoder")?;
        let mut enc = Self {
            encoder,
            gpu: Some(gpu),
            _device: device,
            sws: ptr::null_mut(),
            frame: ptr::null_mut(),
            max_bytes: 0,
            fps: fps.max(1),
            bitstream: Vec::new(),
            packets: Vec::new(),
        };
        enc.set_bitrate(bitrate_kbps);
        Ok(enc)
    }

    pub fn is_zero_copy(&self) -> bool {
        self.gpu.is_some()
    }

    /// `copy` fills the shared texture (an `ID3D11Texture2D*`, slice 0).
    pub fn encode_d3d11(
        &mut self,
        copy: &mut dyn FnMut(*mut c_void, u32) -> Result<()>,
    ) -> Result<EncodedFrame> {
        let started = Instant::now();
        let gpu = self.gpu.as_mut().ok_or_else(|| anyhow!("not a D3D11 encoder"))?;
        copy(gpu.texture_ptr(), 0)?;
        let rate = RateControl {
            maximum_bitstream_size: self.max_bytes,
        };
        gpu.encode(self.encoder, &rate)?;
        self.finish(started)
    }
}
