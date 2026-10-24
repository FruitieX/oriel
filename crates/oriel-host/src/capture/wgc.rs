//! Capture with Windows Graphics Capture: unlike Desktop Duplication it
//! can leave the cursor out of the picture even when Windows draws it
//! there (a software cursor, as with some multi-monitor setups), so only
//! the client's own pointer shows. It cannot capture the lock screen or
//! UAC prompts, and may not start in a process running as SYSTEM.

use std::{
    ffi::c_void,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use oriel_media::HwDevice;
use oriel_proto::CursorShape;
use windows::{
    Graphics::{
        Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession},
        DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat},
        SizeInt32,
    },
    Win32::{
        Graphics::{
            Direct3D11::{
                D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
                D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, ID3D11Device, ID3D11DeviceContext,
                ID3D11Texture2D,
            },
            Dxgi::{DXGI_OUTPUT_DESC, IDXGIDevice},
        },
        System::{
            Com::{COINIT_MULTITHREADED, CoInitializeEx},
            WinRT::{
                Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess},
                Graphics::Capture::IGraphicsCaptureItemInterop,
            },
        },
    },
    core::{Interface, factory},
};

use super::{
    Capturer, Geometry,
    dxgi::{
        OutputEntry, create_device, rect_size, select_output, system_cursor, texture,
        virtual_output, wide_to_string, win32_shape,
    },
};
use crate::cursor::CursorPos;

pub struct WgcCapturer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    desc: DXGI_OUTPUT_DESC,
    size: (u32, u32),
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    latest: ID3D11Texture2D,
    staging: Option<ID3D11Texture2D>,
    cpu: Vec<u8>,
    cpu_fresh: bool,
    /// The cursor whose shape was last sent (as a number).
    cursor: usize,
    new_shape: Option<CursorShape>,
    virtual_monitor: Option<std::sync::Arc<super::sudovda::VirtualMonitor>>,
}

impl WgcCapturer {
    pub fn open(display: Option<u32>) -> Result<Self> {
        Self::open_entry(select_output(display)?, None)
    }

    pub fn open_virtual(monitor: std::sync::Arc<super::sudovda::VirtualMonitor>) -> Result<Self> {
        Self::open_entry(virtual_output(&monitor)?, Some(monitor))
    }

    fn open_entry(
        entry: OutputEntry,
        virtual_monitor: Option<std::sync::Arc<super::sudovda::VirtualMonitor>>,
    ) -> Result<Self> {
        // WinRT wants the thread in an apartment; one already set up stays.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        if !GraphicsCaptureSession::IsSupported().unwrap_or(false) {
            bail!("Windows Graphics Capture is not supported here");
        }
        let (device, context) = create_device(&entry.adapter)?;
        let winrt_device: IDirect3DDevice = unsafe {
            CreateDirect3D11DeviceFromDXGIDevice(&device.cast::<IDXGIDevice>()?)?.cast()?
        };
        let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = unsafe { interop.CreateForMonitor(entry.desc.Monitor) }
            .context("capture item for the monitor")?;
        let size = rect_size(&entry.desc.DesktopCoordinates);
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            SizeInt32 {
                Width: size.0 as i32,
                Height: size.1 as i32,
            },
        )?;
        let session = pool.CreateCaptureSession(&item)?;
        // Older Windows lack these; the cursor then shows in the picture,
        // and a yellow border around the screen.
        if let Err(e) = session.SetIsCursorCaptureEnabled(false) {
            tracing::warn!("cannot leave the cursor out of the capture: {e}");
        }
        let _ = session.SetIsBorderRequired(false);
        session.StartCapture().context("start capturing")?;
        let latest = texture(&device, size, D3D11_USAGE_DEFAULT, 0)?;
        tracing::info!(
            display = %wide_to_string(&entry.desc.DeviceName),
            width = size.0,
            height = size.1,
            "capturing with Windows Graphics Capture"
        );
        Ok(Self {
            device,
            context,
            desc: entry.desc,
            size,
            pool,
            session,
            latest,
            staging: None,
            cpu: Vec::new(),
            cpu_fresh: false,
            cursor: 0,
            new_shape: None,
            virtual_monitor,
        })
    }
}

impl Drop for WgcCapturer {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

impl Capturer for WgcCapturer {
    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn geometry(&self) -> Geometry {
        let r = self.desc.DesktopCoordinates;
        Geometry {
            left: r.left,
            top: r.top,
            width: self.size.0,
            height: self.size.1,
        }
    }

    fn keep_alive(&self) -> Option<std::sync::Arc<dyn std::any::Any + Send + Sync>> {
        self.virtual_monitor
            .clone()
            .map(|m| m as std::sync::Arc<dyn std::any::Any + Send + Sync>)
    }

    fn hw_device(&self) -> HwDevice {
        HwDevice::D3D11(self.device.as_raw())
    }

    /// Frames arrive only when the screen changes; poll for one.
    fn next_frame(&mut self, timeout: Duration) -> Result<bool> {
        let deadline = Instant::now() + timeout;
        let frame = loop {
            // No frame is an error (a null result) as well.
            if let Ok(frame) = self.pool.TryGetNextFrame() {
                break frame;
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        let content = frame.ContentSize()?;
        if (content.Width as u32, content.Height as u32) != self.size {
            bail!("the display changed size");
        }
        let surface: ID3D11Texture2D = unsafe {
            frame
                .Surface()?
                .cast::<IDirect3DDxgiInterfaceAccess>()?
                .GetInterface()?
        };
        unsafe { self.context.CopyResource(&self.latest, &surface) };
        let _ = frame.Close();
        self.cpu_fresh = false;
        Ok(true)
    }

    fn cpu_frame(&mut self) -> Result<(&[u8], usize)> {
        let (w, h) = self.size;
        let stride = w as usize * 4;
        if !self.cpu_fresh {
            if self.staging.is_none() {
                self.staging = Some(texture(
                    &self.device,
                    self.size,
                    D3D11_USAGE_STAGING,
                    D3D11_CPU_ACCESS_READ.0 as u32,
                )?);
            }
            let staging = self.staging.as_ref().unwrap();
            unsafe {
                self.context.CopyResource(staging, &self.latest);
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                self.context
                    .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
                self.cpu.resize(stride * h as usize, 0);
                for y in 0..h as usize {
                    let src = std::slice::from_raw_parts(
                        (mapped.pData as *const u8).add(y * mapped.RowPitch as usize),
                        stride,
                    );
                    self.cpu[y * stride..(y + 1) * stride].copy_from_slice(src);
                }
                self.context.Unmap(staging, 0);
            }
            self.cpu_fresh = true;
        }
        Ok((&self.cpu, stride))
    }

    fn copy_to_texture(&mut self, dst: *mut c_void, index: u32) -> Result<()> {
        let dst = unsafe { ID3D11Texture2D::from_raw_borrowed(&dst) }
            .context("null destination texture")?;
        unsafe {
            self.context
                .CopySubresourceRegion(dst, index, 0, 0, 0, &self.latest, 0, None);
        }
        Ok(())
    }

    /// The cursor is not in the picture: clients draw it, with its shape
    /// read from Win32 whenever it changes.
    fn cursor(&mut self) -> CursorPos {
        let Some((pos, handle)) = system_cursor(self.desc.DesktopCoordinates) else {
            return CursorPos::default();
        };
        let id = handle.0 as usize;
        if id != 0 && id != self.cursor && self.new_shape.is_none() {
            self.cursor = id;
            self.new_shape = win32_shape(handle);
        }
        pos
    }

    fn cursor_shape(&mut self) -> Option<CursorShape> {
        self.new_shape.take()
    }
}
