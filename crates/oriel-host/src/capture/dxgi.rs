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
                CDS_FULLSCREEN, CDS_TYPE, ChangeDisplaySettingsExW, DEVMODEW,
                DISP_CHANGE_SUCCESSFUL, DM_BITSPERPEL, DM_DISPLAYFREQUENCY, DM_PELSHEIGHT,
                DM_PELSWIDTH, ENUM_CURRENT_SETTINGS, ENUM_DISPLAY_SETTINGS_MODE,
                EnumDisplaySettingsW, GetMonitorInfoW, MONITORINFO,
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

/// Resolutions a display supports (32-bit colour, 800×600 and up), largest
/// first.
fn display_modes(device_name: &[u16; 32]) -> Vec<(u32, u32)> {
    let mut modes = Vec::new();
    for i in 0.. {
        let mut mode = DEVMODEW {
            dmSize: size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        let ok = unsafe {
            EnumDisplaySettingsW(
                PCWSTR(device_name.as_ptr()),
                ENUM_DISPLAY_SETTINGS_MODE(i),
                &mut mode,
            )
        };
        if !ok.as_bool() {
            break;
        }
        let size = (mode.dmPelsWidth, mode.dmPelsHeight);
        if mode.dmBitsPerPel == 32 && size.0 >= 800 && size.1 >= 600 && !modes.contains(&size) {
            modes.push(size);
        }
    }
    modes.sort_by(|a, b| (b.0 * b.1).cmp(&(a.0 * a.1)).then(b.0.cmp(&a.0)));
    modes
}

/// A temporary display mode, restored when dropped.
pub struct ModeGuard {
    device: [u16; 32],
    mode: DEVMODEW,
    /// Times the mode was set again without sticking, and when to look
    /// next: each change restarts capture, so a mode Windows keeps
    /// dropping must not be retried every few seconds.
    retries: std::cell::Cell<u32>,
    next_try: std::cell::Cell<std::time::Instant>,
    /// Restore the display's own mode when dropped.
    restore: std::cell::Cell<bool>,
}

// DEVMODEW holds no pointers.
unsafe impl Send for ModeGuard {}

impl Drop for ModeGuard {
    fn drop(&mut self) {
        if !self.restore.get() {
            return;
        }
        unsafe {
            let _ = ChangeDisplaySettingsExW(
                PCWSTR(self.device.as_ptr()),
                None,
                None,
                CDS_TYPE(0),
                None,
            );
        }
        tracing::info!("restored the display mode");
    }
}

impl super::DisplayMode for ModeGuard {
    fn size(&self) -> (u32, u32) {
        (self.mode.dmPelsWidth, self.mode.dmPelsHeight)
    }

    fn keep(&self) {
        self.restore.set(false);
    }

    fn reapply(&self) -> bool {
        let mut current = DEVMODEW {
            dmSize: size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        // Fails while the monitor is gone (asleep on DisplayPort).
        let ok = unsafe {
            EnumDisplaySettingsW(
                PCWSTR(self.device.as_ptr()),
                ENUM_CURRENT_SETTINGS,
                &mut current,
            )
        };
        // The size alone: drivers may report the refresh rate slightly
        // differently from what was set.
        let wanted = (self.mode.dmPelsWidth, self.mode.dmPelsHeight);
        let now = (current.dmPelsWidth, current.dmPelsHeight);
        if !ok.as_bool() || current.dmPelsWidth == 0 {
            return false;
        }
        if now == wanted {
            self.retries.set(0);
            return false;
        }
        let t = std::time::Instant::now();
        if t < self.next_try.get() {
            return false;
        }
        let retries = self.retries.get() + 1;
        self.retries.set(retries);
        // 2 s, 4 s, 8 s ... up to 5 minutes between attempts.
        let wait = std::time::Duration::from_secs((1u64 << retries.min(8)).min(300));
        self.next_try.set(t + wait);
        let result = unsafe {
            ChangeDisplaySettingsExW(
                PCWSTR(self.device.as_ptr()),
                Some(&self.mode),
                None,
                CDS_FULLSCREEN,
                None,
            )
        };
        tracing::info!(
            ?now,
            ?wanted,
            ?result,
            "the display mode was reset; set it again"
        );
        result == DISP_CHANGE_SUCCESSFUL
    }
}

/// Switch `display` to `width`×`height`, or the closest size it supports
/// (highest refresh rate available), until the guard is dropped.
pub fn set_display_mode(display: Option<u32>, width: u32, height: u32) -> Result<ModeGuard> {
    crate::win::follow_input_desktop();
    let outputs = outputs()?;
    let entry = match display {
        Some(i) => outputs
            .into_iter()
            .nth(i as usize)
            .with_context(|| format!("no display {i}"))?,
        None => outputs.into_iter().next().context("no displays attached")?,
    };
    let device = entry.desc.DeviceName;
    let mut modes: Vec<DEVMODEW> = Vec::new();
    for i in 0.. {
        let mut mode = DEVMODEW {
            dmSize: size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        let ok = unsafe {
            EnumDisplaySettingsW(
                PCWSTR(device.as_ptr()),
                ENUM_DISPLAY_SETTINGS_MODE(i),
                &mut mode,
            )
        };
        if !ok.as_bool() {
            break;
        }
        if mode.dmBitsPerPel == 32 {
            modes.push(mode);
        }
    }
    let sizes: Vec<(u32, u32)> = modes
        .iter()
        .map(|m| (m.dmPelsWidth, m.dmPelsHeight))
        .collect();
    let size =
        super::closest_mode(&sizes, (width, height)).context("the display lists no modes")?;
    let mut mode = modes
        .into_iter()
        .filter(|m| (m.dmPelsWidth, m.dmPelsHeight) == size)
        .max_by_key(|m| m.dmDisplayFrequency)
        .expect("a mode of the chosen size");
    mode.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY | DM_BITSPERPEL;
    let result = unsafe {
        ChangeDisplaySettingsExW(
            PCWSTR(device.as_ptr()),
            Some(&mode),
            None,
            CDS_FULLSCREEN,
            None,
        )
    };
    if result != DISP_CHANGE_SUCCESSFUL {
        bail!("Windows refused {}×{} ({result:?})", size.0, size.1);
    }
    tracing::info!(
        device = %wide_to_string(&device),
        asked = ?(width, height),
        width = size.0,
        height = size.1,
        hz = mode.dmDisplayFrequency,
        "changed the display mode"
    );
    Ok(ModeGuard {
        device,
        mode,
        retries: Default::default(),
        next_try: std::cell::Cell::new(std::time::Instant::now()),
        restore: std::cell::Cell::new(true),
    })
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
                modes: display_modes(&o.desc.DeviceName),
            }
        })
        .collect())
}

/// One monitor's desktop duplication, shared by the sessions capturing
/// it: Windows allows one per output and process.
struct Duplication {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    desc: DXGI_OUTPUT_DESC,
    size: (u32, u32),
    /// The virtual monitor being captured, kept while this lives.
    virtual_monitor: Option<std::sync::Arc<super::sudovda::VirtualMonitor>>,
    state: parking_lot::Mutex<DupState>,
}

// The device is multithread-protected; the rest is behind the mutex.
unsafe impl Send for Duplication {}
unsafe impl Sync for Duplication {}

struct DupState {
    /// `None` once lost (mode change, desktop switch): every capturer then
    /// reopens, and the first one duplicates the output again.
    dup: Option<IDXGIOutputDuplication>,
    /// The newest image, and how many have been captured.
    latest: ID3D11Texture2D,
    seq: u64,
    shape: Option<CursorShape>,
    shape_seq: u64,
    shape_buf: Vec<u8>,
    /// Whether duplication last reported the pointer visible on this
    /// output. Windows draws a software cursor into the image instead (some
    /// display modes and setups): duplication then reports it hidden.
    pointer_visible: Option<bool>,
}

/// Duplications in use, by output name.
static DUPLICATIONS: parking_lot::Mutex<Vec<([u16; 32], std::sync::Weak<Duplication>)>> =
    parking_lot::Mutex::new(Vec::new());

impl DupState {
    fn read_pointer_shape(&mut self, size: u32) -> Result<()> {
        let dup = self.dup.as_ref().context("duplication lost")?;
        self.shape_buf.resize(size as usize, 0);
        let mut needed = 0u32;
        let mut info = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
        unsafe {
            dup.GetFramePointerShape(
                size,
                self.shape_buf.as_mut_ptr() as *mut c_void,
                &mut needed,
                &mut info,
            )?;
        }
        if let Some(shape) = convert_shape(&info, &self.shape_buf) {
            self.shape = Some(shape);
            self.shape_seq += 1;
        }
        Ok(())
    }
}

pub struct DxgiCapturer {
    dup: std::sync::Arc<Duplication>,
    /// The last image and cursor shape this capturer passed on.
    seen: u64,
    seen_shape: u64,
    staging: Option<ID3D11Texture2D>,
    cpu: Vec<u8>,
    cpu_fresh: bool,
    /// A shape read from Win32 (see `cursor`), sent before duplication's.
    new_shape: Option<CursorShape>,
    /// The cursor was last found drawn into the image (logged on change).
    cursor_in_image: bool,
    /// The cursor whose shape was last read from Win32 (as a number).
    win32_cursor: usize,
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
        Self::open_entry(entry, None)
    }

    /// Capture a virtual monitor.
    pub fn open_virtual(monitor: std::sync::Arc<super::sudovda::VirtualMonitor>) -> Result<Self> {
        crate::win::follow_input_desktop();
        let entry = outputs()?
            .into_iter()
            .find(|o| o.desc.DeviceName == monitor.device_name)
            .context("the virtual monitor is not attached to the desktop")?;
        Self::open_entry(entry, Some(monitor))
    }

    fn open_entry(
        entry: OutputEntry,
        virtual_monitor: Option<std::sync::Arc<super::sudovda::VirtualMonitor>>,
    ) -> Result<Self> {
        let mut all = DUPLICATIONS.lock();
        all.retain(|(_, d)| d.strong_count() > 0);
        let existing = all
            .iter()
            .filter(|(name, _)| *name == entry.desc.DeviceName)
            .filter_map(|(_, d)| d.upgrade())
            .find(|d| d.state.lock().dup.is_some());
        let dup = match existing {
            Some(d) => d,
            None => {
                let d = std::sync::Arc::new(Self::duplicate(entry, virtual_monitor)?);
                all.push((d.desc.DeviceName, std::sync::Arc::downgrade(&d)));
                d
            }
        };
        Ok(Self {
            dup,
            seen: 0,
            seen_shape: 0,
            staging: None,
            cpu: Vec::new(),
            cpu_fresh: false,
            new_shape: None,
            cursor_in_image: false,
            win32_cursor: 0,
        })
    }

    fn duplicate(
        entry: OutputEntry,
        virtual_monitor: Option<std::sync::Arc<super::sudovda::VirtualMonitor>>,
    ) -> Result<Duplication> {
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
        Ok(Duplication {
            device,
            context,
            desc: entry.desc,
            size,
            virtual_monitor,
            state: parking_lot::Mutex::new(DupState {
                dup: Some(dup),
                latest,
                seq: 0,
                shape: None,
                shape_seq: 0,
                shape_buf: Vec::new(),
                pointer_visible: None,
            }),
        })
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

/// The foreground window covers its whole monitor (a fullscreen game).
fn foreground_fills_monitor() -> bool {
    use windows::Win32::{
        Graphics::Gdi::{MONITOR_DEFAULTTONEAREST, MonitorFromWindow},
        UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowRect},
    };
    unsafe {
        let window = GetForegroundWindow();
        if window.is_invalid() {
            return false;
        }
        let mut rect = RECT::default();
        if GetWindowRect(window, &mut rect).is_err() {
            return false;
        }
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST), &mut info)
            .as_bool()
        {
            return false;
        }
        let m = info.rcMonitor;
        rect.left <= m.left && rect.top <= m.top && rect.right >= m.right && rect.bottom >= m.bottom
    }
}

/// A cursor's image from Win32, converted like duplication's shapes.
fn win32_shape(cursor: windows::Win32::UI::WindowsAndMessaging::HCURSOR) -> Option<CursorShape> {
    use windows::Win32::{
        Graphics::Gdi::{
            BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteObject, GetDC,
            GetDIBits, GetObjectW, HBITMAP, HGDIOBJ, ReleaseDC,
        },
        UI::WindowsAndMessaging::{GetIconInfo, HICON, ICONINFO},
    };
    unsafe {
        let mut icon = ICONINFO::default();
        GetIconInfo(HICON(cursor.0), &mut icon).ok()?;
        let (mask, color) = (icon.hbmMask, icon.hbmColor);
        let size = |bm: HBITMAP| {
            let mut b = BITMAP::default();
            let n = GetObjectW(
                HGDIOBJ(bm.0),
                size_of::<BITMAP>() as i32,
                Some((&mut b as *mut BITMAP).cast()),
            );
            (n != 0).then_some((b.bmWidth.max(0) as usize, b.bmHeight.max(0) as usize))
        };
        let dc = GetDC(None);
        // Rows top-down, `bits` per pixel, DWORD-aligned.
        let read = |bm: HBITMAP, (w, h): (usize, usize), bits: u16| -> Option<(Vec<u8>, usize)> {
            let pitch = (w * bits as usize).div_ceil(32) * 4;
            let mut buf = vec![0u8; pitch * h];
            let mut bi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w as i32,
                    biHeight: -(h as i32),
                    biPlanes: 1,
                    biBitCount: bits,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let rows = GetDIBits(
                dc,
                bm,
                0,
                h as u32,
                Some(buf.as_mut_ptr().cast()),
                &mut bi,
                DIB_RGB_COLORS,
            );
            (rows == h as i32).then_some((buf, pitch))
        };
        let shape = (|| {
            let mut info = DXGI_OUTDUPL_POINTER_SHAPE_INFO {
                HotSpot: windows::Win32::Foundation::POINT {
                    x: icon.xHotspot as i32,
                    y: icon.yHotspot as i32,
                },
                ..Default::default()
            };
            if color.is_invalid() {
                // Monochrome: the AND mask above the XOR mask.
                let (w, h2) = size(mask)?;
                let (buf, pitch) = read(mask, (w, h2), 1)?;
                info.Type = DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32;
                (info.Width, info.Height, info.Pitch) = (w as u32, h2 as u32, pitch as u32);
                return convert_shape(&info, &buf);
            }
            let (w, h) = size(color)?;
            let (mut buf, pitch) = read(color, (w, h), 32)?;
            (info.Width, info.Height, info.Pitch) = (w as u32, h as u32, pitch as u32);
            if buf.chunks_exact(4).any(|px| px[3] != 0) {
                info.Type = DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 as u32;
            } else {
                // No alpha: the AND mask says which pixels are XORed onto
                // the screen (alpha 0xFF in a masked-colour shape).
                let (m, mpitch) = read(mask, (w, h), 32)?;
                for y in 0..h {
                    for x in 0..w {
                        if m[y * mpitch + x * 4] != 0 {
                            buf[y * pitch + x * 4 + 3] = 0xFF;
                        }
                    }
                }
                info.Type = DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 as u32;
            }
            convert_shape(&info, &buf)
        })();
        ReleaseDC(None, dc);
        let _ = DeleteObject(HGDIOBJ(mask.0));
        if !color.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(color.0));
        }
        shape
    }
}

impl Capturer for DxgiCapturer {
    fn size(&self) -> (u32, u32) {
        self.dup.size
    }

    fn geometry(&self) -> Geometry {
        let r = self.dup.desc.DesktopCoordinates;
        Geometry {
            left: r.left,
            top: r.top,
            width: self.dup.size.0,
            height: self.dup.size.1,
        }
    }

    fn keep_alive(&self) -> Option<std::sync::Arc<dyn std::any::Any + Send + Sync>> {
        self.dup
            .virtual_monitor
            .clone()
            .map(|m| m as std::sync::Arc<dyn std::any::Any + Send + Sync>)
    }

    fn hw_device(&self) -> HwDevice {
        HwDevice::D3D11(self.dup.device.as_raw())
    }

    /// Another capturer of the same output may have captured a newer
    /// image already; otherwise wait for one.
    fn next_frame(&mut self, timeout: Duration) -> Result<bool> {
        let shared = self.dup.clone();
        let mut s = shared.state.lock();
        if s.seq > self.seen {
            self.seen = s.seq;
            self.cpu_fresh = false;
            return Ok(true);
        }
        let Some(dup) = s.dup.clone() else {
            bail!("desktop duplication lost (mode change or desktop switch)")
        };
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        let ms = timeout.as_millis().min(u32::MAX as u128) as u32;
        match unsafe { dup.AcquireNextFrame(ms, &mut info, &mut resource) } {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(false),
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                s.dup = None;
                bail!("desktop duplication lost (mode change or desktop switch)")
            }
            Err(e) => return Err(e).context("AcquireNextFrame"),
        }
        let result = (|| -> Result<bool> {
            if info.PointerShapeBufferSize > 0 {
                s.read_pointer_shape(info.PointerShapeBufferSize)?;
            }
            if info.LastMouseUpdateTime != 0 {
                s.pointer_visible = Some(info.PointerPosition.Visible.as_bool());
            }
            if info.LastPresentTime == 0 {
                return Ok(false);
            }
            // Something presented, but without dirty or move rectangles the
            // image is unchanged (e.g. a game or video presenting the same
            // frame); sending it again would only keep the stream busy.
            if info.TotalMetadataBufferSize == 0 && s.seq > 0 {
                return Ok(false);
            }
            let frame: ID3D11Texture2D = resource.context("no frame resource")?.cast()?;
            unsafe { shared.context.CopyResource(&s.latest, &frame) };
            s.seq += 1;
            self.seen = s.seq;
            self.cpu_fresh = false;
            Ok(true)
        })();
        unsafe {
            let _ = dup.ReleaseFrame();
        }
        result
    }

    fn cpu_frame(&mut self) -> Result<(&[u8], usize)> {
        let (w, h) = self.dup.size;
        let stride = w as usize * 4;
        if !self.cpu_fresh {
            if self.staging.is_none() {
                self.staging = Some(texture(
                    &self.dup.device,
                    self.dup.size,
                    D3D11_USAGE_STAGING,
                    D3D11_CPU_ACCESS_READ.0 as u32,
                )?);
            }
            let staging = self.staging.as_ref().unwrap();
            let context = &self.dup.context;
            unsafe {
                // Under the lock: another capturer may be replacing it.
                context.CopyResource(staging, &self.dup.state.lock().latest);
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
                self.cpu.resize(stride * h as usize, 0);
                for y in 0..h as usize {
                    let src = std::slice::from_raw_parts(
                        (mapped.pData as *const u8).add(y * mapped.RowPitch as usize),
                        stride,
                    );
                    self.cpu[y * stride..(y + 1) * stride].copy_from_slice(src);
                }
                context.Unmap(staging, 0);
            }
            self.cpu_fresh = true;
        }
        Ok((&self.cpu, stride))
    }

    fn copy_to_texture(&mut self, dst: *mut c_void, index: u32) -> Result<()> {
        let dst = unsafe { ID3D11Texture2D::from_raw_borrowed(&dst) }
            .context("null destination texture")?;
        let s = self.dup.state.lock();
        unsafe {
            self.dup
                .context
                .CopySubresourceRegion(dst, index, 0, 0, 0, &s.latest, 0, None);
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
        // 3D viewports) take the mouse; with one monitor a fullscreen game's
        // confinement is the whole desktop, so a hidden cursor over a
        // window covering its monitor counts too.
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
        let origin = self.dup.desc.DesktopCoordinates;
        let size = self.dup.size;
        let pointer_visible = self.dup.state.lock().pointer_visible;
        let (x, y) = (
            info.ptScreenPos.x - origin.left,
            info.ptScreenPos.y - origin.top,
        );
        // Showing, on this output, yet hidden to duplication: Windows draws
        // it into the picture (a software cursor) and duplication sends no
        // shape. Clients still show their own pointer, which follows the
        // mouse without the stream's delay: read its shape from Win32.
        let on_output =
            (0..size.0 as i32).contains(&x) && (0..size.1 as i32).contains(&y);
        let in_image = visible && on_output && pointer_visible == Some(false);
        if in_image != self.cursor_in_image {
            self.cursor_in_image = in_image;
            self.win32_cursor = 0;
            tracing::info!(in_image, "cursor drawn into the picture by Windows");
        }
        let handle = info.hCursor.0 as usize;
        if in_image && handle != 0 && handle != self.win32_cursor && self.new_shape.is_none() {
            self.win32_cursor = handle;
            self.new_shape = win32_shape(info.hCursor);
        }
        CursorPos {
            visible,
            captured: !visible && (clipped || foreground_fills_monitor()),
            x,
            y,
        }
    }

    fn cursor_shape(&mut self) -> Option<CursorShape> {
        if let Some(shape) = self.new_shape.take() {
            return Some(shape);
        }
        // Duplication sends a shape only when it changes: a capturer that
        // joined later still takes the current one.
        let s = self.dup.state.lock();
        if s.shape_seq > self.seen_shape {
            self.seen_shape = s.shape_seq;
            return s.shape.clone();
        }
        None
    }
}

#[allow(dead_code)]
fn _assert_types(_: IDXGIOutputDuplication, _: IDXGIOutput1) {}
