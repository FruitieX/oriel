//! Virtual monitors through the SudoVDA indirect display driver
//! (https://github.com/SudoMaker/SudoVDA, as used by Apollo). The driver
//! is driven by IOCTLs on its device interface; it removes every virtual
//! monitor when it has not been pinged for its watchdog timeout (3 s by
//! default), so a thread pings it while monitors exist.

use std::{
    sync::{Arc, Weak},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use oriel_proto::VirtualMode;
use parking_lot::Mutex;
use windows::{
    Win32::{
        Devices::{
            DeviceAndDriverInstallation::{
                DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, SP_DEVICE_INTERFACE_DATA,
                SP_DEVICE_INTERFACE_DETAIL_DATA_W, SetupDiDestroyDeviceInfoList,
                SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
                SetupDiGetDeviceInterfaceDetailW,
            },
            Display::{
                DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_MODE_INFO,
                DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SOURCE_DEVICE_NAME,
                DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QDC_ONLY_ACTIVE_PATHS,
                QueryDisplayConfig,
            },
        },
        Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, LUID},
        Graphics::Gdi::{
            CDS_TYPE, ChangeDisplaySettingsExW, DEVMODEW, DISP_CHANGE_SUCCESSFUL,
            DM_DISPLAYFREQUENCY, DM_PELSHEIGHT, DM_PELSWIDTH, ENUM_CURRENT_SETTINGS,
            EnumDisplaySettingsW,
        },
        Storage::FileSystem::{
            CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        },
        System::IO::DeviceIoControl,
    },
    core::{GUID, PCWSTR},
};

// CTL_CODE(FILE_DEVICE_UNKNOWN, function, METHOD_BUFFERED, FILE_ANY_ACCESS)
const IOCTL_ADD: u32 = 0x22_2000;
const IOCTL_REMOVE: u32 = 0x22_2004;
const IOCTL_GET_WATCHDOG: u32 = 0x22_200C;
const IOCTL_PING: u32 = 0x22_2220;
const IOCTL_GET_PROTOCOL_VERSION: u32 = 0x22_23FC;

/// {e5bcc234-1e0c-418a-a0d4-ef8b7501414d}
const INTERFACE: GUID = GUID::from_u128(0xe5bcc234_1e0c_418a_a0d4_ef8b7501414d);
/// The monitor oriel adds; a fixed id lets Windows remember where the user
/// placed it.
const MONITOR: GUID = GUID::from_u128(0x6f72_6965_6c00_4dee_8a3e_9b2c11d0e5a1);

#[repr(C)]
struct AddParams {
    width: u32,
    height: u32,
    /// Hz below 1000, else millihertz.
    refresh_rate: u32,
    monitor: GUID,
    device_name: [u8; 14],
    serial: [u8; 14],
}

#[repr(C)]
#[derive(Default)]
struct AddOut {
    adapter: LUID,
    target_id: u32,
}

#[repr(C)]
#[derive(Default)]
struct WatchdogOut {
    timeout: u32,
    countdown: u32,
}

#[repr(C)]
#[derive(Default, Debug)]
struct ProtocolVersion {
    major: u8,
    minor: u8,
    incremental: u8,
    test_build: u8,
}

const _: () = assert!(size_of::<AddParams>() == 56 && size_of::<AddOut>() == 12);

struct Driver(HANDLE);

// The handle is only used for synchronous IOCTLs.
unsafe impl Send for Driver {}
unsafe impl Sync for Driver {}

impl Drop for Driver {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

impl Driver {
    fn ioctl<I, O>(&self, code: u32, input: Option<&I>, output: Option<&mut O>) -> Result<()> {
        let mut returned = 0u32;
        unsafe {
            DeviceIoControl(
                self.0,
                code,
                input.map(|i| i as *const I as *const _),
                input.map_or(0, |_| size_of::<I>() as u32),
                output.map(|o| o as *mut O as *mut _),
                if size_of::<O>() > 0 {
                    size_of::<O>() as u32
                } else {
                    0
                },
                Some(&mut returned),
                None,
            )?;
        }
        Ok(())
    }

    fn ping(&self) -> Result<()> {
        self.ioctl::<(), ()>(IOCTL_PING, None, None)
    }
}

static DRIVER: Mutex<Option<Weak<Driver>>> = Mutex::new(None);

/// The driver, opened on first use (with its watchdog ping thread).
fn driver() -> Result<Arc<Driver>> {
    let mut current = DRIVER.lock();
    if let Some(d) = current.as_ref().and_then(Weak::upgrade) {
        return Ok(d);
    }
    let driver = Arc::new(Driver(open_device()?));
    let mut version = ProtocolVersion::default();
    driver
        .ioctl::<(), _>(IOCTL_GET_PROTOCOL_VERSION, None, Some(&mut version))
        .context("query the SudoVDA protocol version")?;
    // Written against protocol 0.2; minor versions only add IOCTLs.
    if version.major != 0 || version.minor < 2 {
        bail!("unsupported SudoVDA driver protocol {version:?}");
    }
    let mut watchdog = WatchdogOut::default();
    driver.ioctl::<(), _>(IOCTL_GET_WATCHDOG, None, Some(&mut watchdog))?;
    if watchdog.timeout > 0 {
        let interval = Duration::from_millis(watchdog.timeout as u64 * 1000 / 3);
        let weak = Arc::downgrade(&driver);
        std::thread::Builder::new()
            .name("oriel-sudovda".into())
            .spawn(move || {
                while let Some(d) = weak.upgrade() {
                    if let Err(e) = d.ping() {
                        tracing::warn!("SudoVDA ping failed: {e:#}");
                    }
                    drop(d);
                    std::thread::sleep(interval);
                }
            })?;
    }
    *current = Some(Arc::downgrade(&driver));
    Ok(driver)
}

fn open_device() -> Result<HANDLE> {
    unsafe {
        let set = SetupDiGetClassDevsW(
            Some(&INTERFACE),
            PCWSTR::null(),
            None,
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
        .context("the SudoVDA driver is not installed")?;
        let mut found = None;
        for i in 0.. {
            let mut data = SP_DEVICE_INTERFACE_DATA {
                cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
                ..Default::default()
            };
            if SetupDiEnumDeviceInterfaces(set, None, &INTERFACE, i, &mut data).is_err() {
                break;
            }
            let mut size = 0u32;
            let _ = SetupDiGetDeviceInterfaceDetailW(set, &data, None, 0, Some(&mut size), None);
            // u32 storage keeps the detail struct aligned.
            let mut buf = vec![0u32; (size as usize).div_ceil(4).max(2)];
            let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
            (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
            if SetupDiGetDeviceInterfaceDetailW(set, &data, Some(detail), size, None, None).is_err()
            {
                continue;
            }
            let path = PCWSTR((*detail).DevicePath.as_ptr());
            if let Ok(h) = CreateFileW(
                path,
                (GENERIC_READ | GENERIC_WRITE).0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            ) {
                found = Some(h);
                break;
            }
        }
        let _ = SetupDiDestroyDeviceInfoList(set);
        found.context("the SudoVDA driver is not installed (no device)")
    }
}

/// Whether the driver is installed and usable.
pub fn available() -> bool {
    driver().is_ok()
}

/// A virtual monitor; removed when dropped.
pub struct VirtualMonitor {
    driver: Arc<Driver>,
    pub mode: VirtualMode,
    /// GDI name, e.g. `\\.\DISPLAY5`.
    pub device_name: [u16; 32],
}

impl Drop for VirtualMonitor {
    fn drop(&mut self) {
        let params = MONITOR;
        if let Err(e) = self
            .driver
            .ioctl::<_, ()>(IOCTL_REMOVE, Some(&params), None)
        {
            tracing::warn!("cannot remove the virtual monitor: {e:#}");
        } else {
            tracing::info!("removed the virtual monitor");
        }
    }
}

static CURRENT: Mutex<Option<Weak<VirtualMonitor>>> = Mutex::new(None);

/// The virtual monitor in `mode`, created if needed. A monitor in another
/// mode must have been dropped first.
pub fn acquire(mode: VirtualMode) -> Result<Arc<VirtualMonitor>> {
    let mut current = CURRENT.lock();
    if let Some(m) = current.as_ref().and_then(Weak::upgrade) {
        if m.mode == mode {
            return Ok(m);
        }
        bail!("another virtual monitor is still in use");
    }
    let driver = driver()?;
    let mut params = AddParams {
        width: mode.width,
        height: mode.height,
        refresh_rate: mode.refresh_hz,
        monitor: MONITOR,
        device_name: [0; 14],
        serial: [0; 14],
    };
    params.device_name[..5].copy_from_slice(b"oriel");
    params.serial[..5].copy_from_slice(b"oriel");
    let mut out = AddOut::default();
    driver
        .ioctl(IOCTL_ADD, Some(&params), Some(&mut out))
        .context("add a virtual monitor")?;
    // Windows takes a moment to attach the new monitor.
    let mut device_name = None;
    for delay in [20, 40, 80, 160, 320, 640, 1000] {
        std::thread::sleep(Duration::from_millis(delay));
        device_name = gdi_name(&out);
        if device_name.is_some() {
            break;
        }
    }
    let monitor = Arc::new(VirtualMonitor {
        driver,
        mode,
        device_name: device_name.context("the virtual monitor did not appear")?,
    });
    apply_mode(&monitor.device_name, mode);
    tracing::info!(
        ?mode,
        name = %String::from_utf16_lossy(&monitor.device_name).trim_end_matches('\0'),
        "created a virtual monitor"
    );
    *current = Some(Arc::downgrade(&monitor));
    Ok(monitor)
}

/// The GDI device name of the monitor the driver reported.
fn gdi_name(out: &AddOut) -> Option<[u16; 32]> {
    unsafe {
        let (mut paths, mut modes) = (0u32, 0u32);
        if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut paths, &mut modes).is_err() {
            return None;
        }
        let mut path_info = vec![DISPLAYCONFIG_PATH_INFO::default(); paths as usize];
        let mut mode_info = vec![DISPLAYCONFIG_MODE_INFO::default(); modes as usize];
        if QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut paths,
            path_info.as_mut_ptr(),
            &mut modes,
            mode_info.as_mut_ptr(),
            None,
        )
        .is_err()
        {
            return None;
        }
        let path = path_info[..paths as usize].iter().find(|p| {
            p.targetInfo.id == out.target_id
                && p.targetInfo.adapterId.LowPart == out.adapter.LowPart
                && p.targetInfo.adapterId.HighPart == out.adapter.HighPart
        })?;
        let mut name = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
        name.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
        name.header.size = size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
        name.header.adapterId = path.sourceInfo.adapterId;
        name.header.id = path.sourceInfo.id;
        if DisplayConfigGetDeviceInfo(&mut name.header) != 0 {
            return None;
        }
        Some(name.viewGdiDeviceName)
    }
}

/// Make sure the monitor runs in `mode` (the driver prefers it, but Windows
/// may restore a remembered one).
fn apply_mode(device: &[u16; 32], mode: VirtualMode) {
    unsafe {
        let mut current = DEVMODEW {
            dmSize: size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        let _ = EnumDisplaySettingsW(PCWSTR(device.as_ptr()), ENUM_CURRENT_SETTINGS, &mut current);
        if (
            current.dmPelsWidth,
            current.dmPelsHeight,
            current.dmDisplayFrequency,
        ) == (mode.width, mode.height, mode.refresh_hz)
        {
            return;
        }
        let wanted = DEVMODEW {
            dmSize: size_of::<DEVMODEW>() as u16,
            dmFields: DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY,
            dmPelsWidth: mode.width,
            dmPelsHeight: mode.height,
            dmDisplayFrequency: mode.refresh_hz,
            ..Default::default()
        };
        let result = ChangeDisplaySettingsExW(
            PCWSTR(device.as_ptr()),
            Some(&wanted),
            None,
            CDS_TYPE(0),
            None,
        );
        if result != DISP_CHANGE_SUCCESSFUL {
            tracing::warn!(?mode, ?result, "cannot set the virtual monitor's mode");
        }
    }
}
