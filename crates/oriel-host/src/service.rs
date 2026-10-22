//! Windows service: runs the host as SYSTEM in the active console session so
//! it can capture and control the lock screen, UAC prompts and the login
//! screen, and restarts it across logons and console switches.
//!
//! The service itself lives in session 0, which has no desktop. It starts
//! `oriel host` with a copy of its own SYSTEM token moved into the console
//! session, and calls `SendSAS` for it (Ctrl+Alt+Del must come from a
//! service).

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    sync::mpsc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0},
        Security::{
            DuplicateTokenEx, SecurityImpersonation, SetTokenInformation, TOKEN_ALL_ACCESS,
            TokenPrimary, TokenSessionId,
        },
        System::{
            RemoteDesktop::WTSGetActiveConsoleSessionId,
            Threading::{
                CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateEventW, CreateProcessAsUserW,
                GetCurrentProcess, OpenProcessToken, PROCESS_INFORMATION, STARTUPINFOW,
                TerminateProcess, WaitForMultipleObjects, WaitForSingleObject,
            },
        },
    },
    core::{HSTRING, PWSTR},
};
use windows_service::{
    define_windows_service,
    service::{
        ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
        ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
    },
    service_control_handler::{self, ServiceControlHandlerResult},
    service_dispatcher,
    service_manager::{ServiceManager, ServiceManagerAccess},
};

pub const SERVICE_NAME: &str = "oriel";
const DISPLAY_NAME: &str = "oriel remote desktop host";
/// The host signals this event to ask the service for Ctrl+Alt+Del.
const SAS_EVENT: &str = "Global\\oriel-secure-attention";

/// Where the service keeps its identity, pairings and log.
pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("ProgramData").unwrap_or_else(|| "C:\\ProgramData".into());
    PathBuf::from(base).join("oriel")
}

fn install_dir() -> PathBuf {
    let base = std::env::var_os("ProgramFiles").unwrap_or_else(|| "C:\\Program Files".into());
    PathBuf::from(base).join("oriel")
}

fn run(cmd: &str, args: &[&str]) -> Result<()> {
    let status = std::process::Command::new(cmd)
        .args(args)
        .stdout(std::process::Stdio::null())
        .status()
        .with_context(|| format!("run {cmd}"))?;
    if !status.success() {
        bail!("{cmd} {} failed ({status})", args.join(" "));
    }
    Ok(())
}

/// Copy this program to Program Files, register and start the service.
/// `host_args` are passed to `oriel host` (e.g. `--name`).
pub fn install(host_args: &[String]) -> Result<()> {
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .context("open the service manager (run this as Administrator)")?;

    // Stop an existing installation so its files can be replaced.
    if let Ok(existing) = manager.open_service(
        SERVICE_NAME,
        ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
    ) {
        let _ = existing.stop();
        wait_stopped(&existing);
        existing.delete()?;
        drop(existing);
        std::thread::sleep(Duration::from_millis(500));
    }

    let source = std::env::current_exe()?
        .parent()
        .context("no program directory")?
        .to_path_buf();
    let target = install_dir();
    std::fs::create_dir_all(&target)?;
    for entry in std::fs::read_dir(&source)? {
        let path = entry?.path();
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default();
        if ext.eq_ignore_ascii_case("exe") || ext.eq_ignore_ascii_case("dll") {
            let dest = target.join(path.file_name().unwrap());
            std::fs::copy(&path, &dest)
                .with_context(|| format!("copy {} to {}", path.display(), dest.display()))?;
        }
    }
    let exe = target.join("oriel.exe");
    println!("Installed {}", exe.display());

    // Identity and pairings: SYSTEM and Administrators only.
    let data = data_dir();
    std::fs::create_dir_all(&data)?;
    let data_str = data.to_string_lossy().to_string();
    run(
        "icacls",
        &[
            &data_str,
            "/inheritance:r",
            "/grant:r",
            "*S-1-5-18:(OI)(CI)F",
            "/grant:r",
            "*S-1-5-32-544:(OI)(CI)F",
        ],
    )?;

    // Let services generate Ctrl+Alt+Del (SendSAS).
    run(
        "reg",
        &[
            "add",
            "HKLM\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Policies\\System",
            "/v",
            "SoftwareSASGeneration",
            "/t",
            "REG_DWORD",
            "/d",
            "1",
            "/f",
        ],
    )?;

    let exe_str = exe.to_string_lossy().to_string();
    let _ = run(
        "netsh",
        &["advfirewall", "firewall", "delete", "rule", "name=oriel"],
    );
    run(
        "netsh",
        &[
            "advfirewall",
            "firewall",
            "add",
            "rule",
            "name=oriel",
            "dir=in",
            "action=allow",
            "protocol=UDP",
            &format!("localport={}", oriel_proto::DEFAULT_PORT),
            &format!("program={exe_str}"),
            "profile=private,domain",
        ],
    )?;

    let mut launch_arguments: Vec<OsString> = vec!["service".into(), "run".into(), "--".into()];
    launch_arguments.extend(host_args.iter().map(OsString::from));
    let info = ServiceInfo {
        name: SERVICE_NAME.into(),
        display_name: DISPLAY_NAME.into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe,
        launch_arguments,
        dependencies: vec![],
        account_name: None, // LocalSystem
        account_password: None,
    };
    let service = manager.create_service(&info, ServiceAccess::START | ServiceAccess::CHANGE_CONFIG)?;
    service.set_description("Shares this computer's display with paired oriel clients")?;
    service.start::<&str>(&[])?;
    println!("Service started. Pair a client with: oriel service pair");
    Ok(())
}

fn wait_stopped(service: &windows_service::service::Service) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        match service.query_status() {
            Ok(s) if s.current_state == ServiceState::Stopped => return,
            Err(_) => return,
            _ => std::thread::sleep(Duration::from_millis(200)),
        }
    }
}

pub fn uninstall() -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("open the service manager (run this as Administrator)")?;
    let service = manager.open_service(
        SERVICE_NAME,
        ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
    )?;
    let _ = service.stop();
    wait_stopped(&service);
    service.delete()?;
    let _ = run(
        "netsh",
        &["advfirewall", "firewall", "delete", "rule", "name=oriel"],
    );
    println!(
        "Service removed. {} and {} were left in place.",
        install_dir().display(),
        data_dir().display()
    );
    Ok(())
}

define_windows_service!(ffi_service_main, service_main);

/// Entry point for `oriel service run` (started by the service manager).
pub fn run_dispatcher() -> Result<()> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
        .context("not started by the service manager (use `oriel service install`)")?;
    Ok(())
}

enum Control {
    Stop,
    SessionChanged,
}

fn service_main(args: Vec<OsString>) {
    if let Err(e) = supervise(args) {
        tracing::error!("service failed: {e:#}");
    }
}

fn status(state: ServiceState, accept: ServiceControlAccept) -> ServiceStatus {
    ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: accept,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::from_secs(5),
        process_id: None,
    }
}

fn supervise(args: Vec<OsString>) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    let handle = service_control_handler::register(SERVICE_NAME, move |event| match event {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            let _ = tx.send(Control::Stop);
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::SessionChange(_) => {
            let _ = tx.send(Control::SessionChanged);
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    })?;
    handle.set_service_status(status(
        ServiceState::Running,
        ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN | ServiceControlAccept::SESSION_CHANGE,
    ))?;

    // Everything after "--" in the service's arguments goes to the host.
    let exe = std::env::current_exe()?;
    let host_args: Vec<String> = std::env::args_os()
        .skip_while(|a| a != "--")
        .skip(1)
        .chain(args.into_iter().skip(1))
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let sas_event = unsafe { CreateEventW(None, false, false, &HSTRING::from(SAS_EVENT))? };

    let mut child: Option<(HANDLE, u32)> = None;
    let mut failures = 0u32;
    let mut next_start = Instant::now();
    loop {
        let session = unsafe { WTSGetActiveConsoleSessionId() };
        if child.is_none() && session != u32::MAX && Instant::now() >= next_start {
            match spawn_host(&exe, &host_args, session) {
                Ok(process) => {
                    tracing::info!(session, "started host");
                    child = Some((process, session));
                }
                Err(e) => {
                    failures += 1;
                    tracing::warn!(session, "cannot start host: {e:#}");
                    next_start = Instant::now() + Duration::from_secs(failures.min(10) as u64);
                }
            }
        }

        // Wait for the host to exit, a Ctrl+Alt+Del request, or 500 ms.
        let mut handles = vec![sas_event];
        if let Some((process, _)) = child {
            handles.push(process);
        }
        let waited = unsafe { WaitForMultipleObjects(&handles, false, 500) };
        if waited == WAIT_OBJECT_0 {
            crate::win::send_secure_attention();
        } else if child.is_some() && waited.0 == WAIT_OBJECT_0.0 + 1 {
            let (process, _) = child.take().unwrap();
            unsafe {
                let _ = CloseHandle(process);
            }
            failures += 1;
            tracing::warn!("host exited; restarting");
            next_start = Instant::now() + Duration::from_secs(failures.min(10) as u64);
        }

        match rx.try_recv() {
            Ok(Control::Stop) | Err(mpsc::TryRecvError::Disconnected) => break,
            Ok(Control::SessionChanged) => {
                let active = unsafe { WTSGetActiveConsoleSessionId() };
                if let Some((process, s)) = child
                    && s != active
                {
                    tracing::info!(from = s, to = active, "console session changed; moving host");
                    stop_child(process);
                    child = None;
                    failures = 0;
                    next_start = Instant::now();
                }
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
        if child.is_some() && failures > 0 && next_start + Duration::from_secs(60) < Instant::now() {
            failures = 0;
        }
    }

    if let Some((process, _)) = child {
        stop_child(process);
    }
    unsafe {
        let _ = CloseHandle(sas_event);
    }
    handle.set_service_status(status(ServiceState::Stopped, ServiceControlAccept::empty()))?;
    Ok(())
}

fn stop_child(process: HANDLE) {
    unsafe {
        let _ = TerminateProcess(process, 0);
        let _ = WaitForSingleObject(process, 3000);
        let _ = CloseHandle(process);
    }
}

/// Start `oriel host` as SYSTEM inside `session`.
fn spawn_host(exe: &Path, args: &[String], session: u32) -> Result<HANDLE> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut token)?;
        let mut primary = HANDLE::default();
        let dup = DuplicateTokenEx(
            token,
            TOKEN_ALL_ACCESS,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut primary,
        );
        let _ = CloseHandle(token);
        dup?;
        let result = (|| -> Result<HANDLE> {
            SetTokenInformation(
                primary,
                TokenSessionId,
                &session as *const u32 as *const _,
                size_of::<u32>() as u32,
            )?;
            let mut cmdline = format!("\"{}\" host", exe.display());
            for a in args {
                cmdline.push_str(&format!(" \"{}\"", a.replace('"', "\\\"")));
            }
            let mut cmdline: Vec<u16> = cmdline.encode_utf16().chain(Some(0)).collect();
            let mut desktop: Vec<u16> = "winsta0\\default".encode_utf16().chain(Some(0)).collect();
            let si = STARTUPINFOW {
                cb: size_of::<STARTUPINFOW>() as u32,
                lpDesktop: PWSTR(desktop.as_mut_ptr()),
                ..Default::default()
            };
            let mut pi = PROCESS_INFORMATION::default();
            // The host inherits ORIEL_CONFIG_DIR from the service.
            CreateProcessAsUserW(
                Some(primary),
                None,
                Some(PWSTR(cmdline.as_mut_ptr())),
                None,
                None,
                false,
                CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT,
                None,
                None,
                &si,
                &mut pi,
            )?;
            let _ = CloseHandle(pi.hThread);
            Ok(pi.hProcess)
        })();
        let _ = CloseHandle(primary);
        result
    }
}

/// `oriel service pair`: allow pairing for ten minutes with a fresh PIN.
pub fn allow_pairing() -> Result<String> {
    let pin = oriel_net::pairing::generate_pin();
    let path = data_dir().join(crate::server::PIN_FILE);
    std::fs::write(&path, &pin)
        .with_context(|| format!("write {} (run this as Administrator)", path.display()))?;
    Ok(pin)
}

/// Ask the service (if any) to send Ctrl+Alt+Del. Returns false when no
/// service is listening.
pub fn request_secure_attention() -> bool {
    use windows::Win32::System::Threading::{EVENT_MODIFY_STATE, OpenEventW, SetEvent};
    unsafe {
        match OpenEventW(EVENT_MODIFY_STATE, false, &HSTRING::from(SAS_EVENT)) {
            Ok(event) => {
                let ok = SetEvent(event).is_ok();
                let _ = CloseHandle(event);
                ok
            }
            Err(_) => false,
        }
    }
}
