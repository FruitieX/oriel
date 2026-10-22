//! Desktop Duplication capture (Windows 8+).
//!
//! Each acquired frame is copied into a texture we own and released right
//! away, so the latest image can be encoded at any later time (rate limits,
//! keyframe requests while the desktop is static) without holding DXGI's
//! frame.

use std::{ffi::c_void, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use oriel_media::HwDevice;
use oriel_proto::{CursorShape, DisplayInfo};
use windows::{
    Win32::{
        Foundation::{HMODULE, RECT},
        Graphics::{
            Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0},
            Direct3D11::{
                D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
                D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
                D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device,
                ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
            },
            Dxgi::{
                Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC},
                CreateDXGIFactory1, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_NOT_FOUND,
                DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_POINTER_SHAPE_INFO,
                DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR,
                DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR,
                DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME, DXGI_OUTPUT_DESC, IDXGIAdapter1,
                IDXGIFactory1, IDXGIOutput, IDXGIOutput1, IDXGIOutput5, IDXGIOutputDuplication,
                IDXGIResource,
            },
            Gdi::{
                DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW, GetMonitorInfoW, MONITORINFO,
            },
        },
        UI::WindowsAndMessaging::{
            CURSOR_SHOWING, CURSORINFO, GetClipCursor, GetCursorInfo, GetSystemMetrics,
            MONITORINFOF_PRIMARY, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
            SM_YVIRTUALSCREEN,
        },
    },
    core::{Interface, PCWSTR},
};

use super::{Capturer, Geometry};
use crate::cursor::{self, CursorPos};

struct OutputEntry {
    adapter: IDXGIAdapter1,
    output: IDXGIOutput,
    desc: DXGI_OUTPUT_DESC,
    primary: bool,
}

fn outputs() -> Result<Vec<OutputEntry>> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
    let mut out = Vec::new();
    for a in 0.. {
        let adapter = match unsafe { factory.EnumAdapters1(a) } {
            Ok(a) => a,
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(e) => return Err(e.into()),
        };
        for o in 0.. {
            let output = match unsafe { adapter.EnumOutputs(o) } {
                Ok(o) => o,
                Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(e) => return Err(e.into()),
            };
            let desc = unsafe { output.GetDesc()? };
            if !desc.AttachedToDesktop.as_bool() {
                continue;
            }
            let mut info = MONITORINFO {
                cbSize: size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            let primary = unsafe { GetMonitorInfoW(desc.Monitor, &mut info) }.as_bool()
                && info.dwFlags & MONITORINFOF_PRIMARY != 0;
            out.push(OutputEntry {
                adapter: adapter.clone(),
                output,
                desc,
                primary,
            });
        }
    }
    // Primary first, so display 0 is the primary monitor.
    out.sort_by_key(|o| !o.primary);
    Ok(out)
}

fn wide_to_string(w: &[u16]) -> String {
    let len = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..len])
}

fn refresh_hz(device_name: &[u16; 32]) -> u32 {
    let mut mode = DEVMODEW {
        dmSize: size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    let ok = unsafe {
        EnumDisplaySettingsW(
            PCWSTR(device_name.as_ptr()),
            ENUM_CURRENT_SETTINGS,
            &mut mode,
        )
    };
    if ok.as_bool() {
        mode.dmDisplayFrequency
    } else {
        60
    }
}

fn rect_size(r: &RECT) -> (u32, u32) {
    ((r.right - r.left) as u32, (r.bottom - r.top) as u32)
}

pub fn list_displays() -> Result<Vec<DisplayInfo>> {
    Ok(outputs()?
        .iter()
        .enumerate()
        .map(|(i, o)| {
            let (width, height) = rect_size(&o.desc.DesktopCoordinates);
            DisplayInfo {
                id: i as u32,
                name: wide_to_string(&o.desc.DeviceName),
                width,
                height,
                refresh_hz: refresh_hz(&o.desc.DeviceName),
                primary: o.primary,
            }
        })
        .collect())
}

pub struct DxgiCapturer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    dup: IDXGIOutputDuplication,
    desc: DXGI_OUTPUT_DESC,
    size: (u32, u32),
    latest: ID3D11Texture2D,
    staging: Option<ID3D11Texture2D>,
    cpu: Vec<u8>,
    cpu_fresh: bool,
    new_shape: Option<CursorShape>,
    shape_buf: Vec<u8>,
}

impl DxgiCapturer {
    pub fn open(display: Option<u32>) -> Result<Self> {
        // Desktop switches (lock screen, UAC) need the thread on the input
        // desktop before duplicating it.
        crate::win::follow_input_desktop();
        let outputs = outputs()?;
        let entry = match display {
            Some(i) => outputs
                .into_iter()
                .nth(i as usize)
                .with_context(|| format!("no display {i}"))?,
            None => outputs.into_iter().next().context("no displays attached")?,
        };
        let mut device = None;
        let mut context = None;
        unsafe {
            D3D11CreateDevice(
                &entry.adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .context("create D3D11 device")?;
        }
        let device = device.context("no D3D11 device")?;
        let context = context.context("no D3D11 context")?;
        // Encoders may use the device from their own threads.
        if let Ok(mt) = device.cast::<ID3D11Multithread>() {
            unsafe {
                let _ = mt.SetMultithreadProtected(true);
            }
        }
        let dup = unsafe {
            match entry.output.cast::<IDXGIOutput5>() {
                Ok(o5) => o5.DuplicateOutput1(&device, 0, &[DXGI_FORMAT_B8G8R8A8_UNORM]),
                Err(_) => entry
                    .output
                    .cast::<IDXGIOutput1>()?
                    .DuplicateOutput(&device),
            }
        }
        .context("duplicate output (is another app capturing, or is this a secure desktop?)")?;
        let size = rect_size(&entry.desc.DesktopCoordinates);
        let latest = texture(&device, size, D3D11_USAGE_DEFAULT, 0)?;
        tracing::info!(
            display = %wide_to_string(&entry.desc.DeviceName),
            width = size.0,
            height = size.1,
            "capturing with Desktop Duplication"
        );
        Ok(Self {
            device,
            context,
            dup,
            desc: entry.desc,
            size,
            latest,
            staging: None,
            cpu: Vec::new(),
            cpu_fresh: false,
            new_shape: None,
            shape_buf: Vec::new(),
        })
    }

    fn read_pointer_shape(&mut self, size: u32) -> Result<()> {
        self.shape_buf.resize(size as usize, 0);
        let mut needed = 0u32;
        let mut info = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
        unsafe {
            self.dup.GetFramePointerShape(
                size,
                self.shape_buf.as_mut_ptr() as *mut c_void,
                &mut needed,
                &mut info,
            )?;
        }
        self.new_shape = convert_shape(&info, &self.shape_buf);
        Ok(())
    }
}

fn texture(
    device: &ID3D11Device,
    (w, h): (u32, u32),
    usage: windows::Win32::Graphics::Direct3D11::D3D11_USAGE,
    cpu_access: u32,
) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: usage,
        BindFlags: 0,
        CPUAccessFlags: cpu_access,
        MiscFlags: 0,
    };
    let mut tex = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex))? };
    tex.ok_or_else(|| anyhow!("CreateTexture2D returned nothing"))
}

/// Convert a DXGI pointer shape to straight RGBA.
fn convert_shape(info: &DXGI_OUTDUPL_POINTER_SHAPE_INFO, buf: &[u8]) -> Option<CursorShape> {
    let pitch = info.Pitch as usize;
    let (w, mut h) = (info.Width as usize, info.Height as usize);
    let ty = info.Type as i32;
    if ty == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 {
        h /= 2;
    }
    if w == 0 || h == 0 {
        return None;
    }
    let mut rgba = vec![0u8; w * h * 4];
    let mut invert = vec![false; w * h];
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            let px = &mut rgba[i * 4..i * 4 + 4];
            if ty == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 {
                let bit = |row: usize| (buf[row * pitch + x / 8] >> (7 - x % 8)) & 1;
                match (bit(y), bit(y + h)) {
                    (0, 0) => px.copy_from_slice(&[0, 0, 0, 255]),
                    (0, 1) => px.copy_from_slice(&[255, 255, 255, 255]),
                    (1, 0) => {}
                    _ => {
                        px.copy_from_slice(&[0, 0, 0, 255]);
                        invert[i] = true;
                    }
                }
            } else {
                let s = &buf[y * pitch + x * 4..y * pitch + x * 4 + 4];
                if ty == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 {
                    px.copy_from_slice(&[s[2], s[1], s[0], s[3]]);
                } else if ty == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 {
                    if s[3] == 0 {
                        px.copy_from_slice(&[s[2], s[1], s[0], 255]);
                    } else if s[0] | s[1] | s[2] != 0 {
                        px.copy_from_slice(&[0, 0, 0, 255]);
                        invert[i] = true;
                    }
                }
            }
        }
    }
    if invert.iter().any(|&i| i) {
        cursor::outline_inverted(w as u32, h as u32, &mut rgba, &invert);
    }
    Some(cursor::make_shape(
        w as u32,
        h as u32,
        info.HotSpot.x.max(0) as u32,
        info.HotSpot.y.max(0) as u32,
        rgba,
    ))
}

impl Capturer for DxgiCapturer {
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

    fn hw_device(&self) -> HwDevice {
        HwDevice::D3D11(self.device.as_raw())
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<bool> {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        let ms = timeout.as_millis().min(u32::MAX as u128) as u32;
        match unsafe { self.dup.AcquireNextFrame(ms, &mut info, &mut resource) } {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(false),
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                bail!("desktop duplication lost (mode change or desktop switch)")
            }
            Err(e) => return Err(e).context("AcquireNextFrame"),
        }
        let result = (|| -> Result<bool> {
            if info.PointerShapeBufferSize > 0 {
                self.read_pointer_shape(info.PointerShapeBufferSize)?;
            }
            if info.LastPresentTime == 0 {
                return Ok(false);
            }
            let frame: ID3D11Texture2D = resource.context("no frame resource")?.cast()?;
            unsafe { self.context.CopyResource(&self.latest, &frame) };
            self.cpu_fresh = false;
            Ok(true)
        })();
        unsafe {
            let _ = self.dup.ReleaseFrame();
        }
        result
    }

    fn cpu_frame(&mut self) -> Result<(&[u8], usize)> {
        let stride = self.size.0 as usize * 4;
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
                self.cpu.resize(stride * self.size.1 as usize, 0);
                for y in 0..self.size.1 as usize {
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

    fn cursor(&mut self) -> CursorPos {
        let mut info = CURSORINFO {
            cbSize: size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        if unsafe { GetCursorInfo(&mut info) }.is_err() {
            return CursorPos::default();
        }
        let visible = info.flags.0 & CURSOR_SHOWING.0 != 0;
        // A hidden cursor confined to part of the desktop is how games (and
        // 3D viewports) take the mouse.
        let mut clip = RECT::default();
        let clipped = unsafe {
            GetClipCursor(&mut clip).is_ok() && {
                let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
                let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
                let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
                let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);
                (clip.left, clip.top, clip.right, clip.bottom) != (vx, vy, vx + vw, vy + vh)
            }
        };
        let origin = self.desc.DesktopCoordinates;
        CursorPos {
            visible,
            captured: !visible && clipped,
            x: info.ptScreenPos.x - origin.left,
            y: info.ptScreenPos.y - origin.top,
        }
    }

    fn cursor_shape(&mut self) -> Option<CursorShape> {
        self.new_shape.take()
    }
}

#[allow(dead_code)]
fn _assert_types(_: IDXGIOutputDuplication, _: IDXGIOutput1) {}
