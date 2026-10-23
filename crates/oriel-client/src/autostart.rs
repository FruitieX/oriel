//! Starting oriel when the user logs in, the native way on each platform:
//! the `Run` registry key on Windows, an XDG autostart entry on Linux and a
//! LaunchAgent on macOS. `--background` starts it in the tray (Windows) or
//! minimized.

use anyhow::{Context, Result};

/// The flag the autostart entry passes when oriel should start hidden.
pub const BACKGROUND_FLAG: &str = "--background";

/// Create or remove the autostart entry.
pub fn set(enabled: bool, background: bool) -> Result<()> {
    let args: &[&str] = if background { &[BACKGROUND_FLAG] } else { &[] };
    imp::set(enabled, args)
}

/// Whether the installer set oriel up to start for every user (Windows).
pub fn machine_wide() -> bool {
    #[cfg(windows)]
    return imp::machine_wide();
    #[cfg(not(windows))]
    false
}

/// The program the entry starts: on NixOS the stable `oriel` on PATH
/// rather than a store path that changes with every update.
#[allow(dead_code)]
fn program() -> Result<String> {
    if cfg!(target_os = "linux")
        && let Some(path) = std::env::var_os("PATH")
    {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join("oriel");
            if candidate.is_file() && !candidate.starts_with("/nix/store") {
                return Ok(candidate.display().to_string());
            }
        }
    }
    Ok(std::env::current_exe()
        .context("find the oriel executable")?
        .display()
        .to_string())
}

#[cfg(target_os = "linux")]
mod imp {
    use super::*;

    fn entry() -> Result<std::path::PathBuf> {
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| std::path::Path::new(&h).join(".config")))
            .context("no home directory")?;
        Ok(config.join("autostart").join("oriel.desktop"))
    }

    pub fn set(enabled: bool, args: &[&str]) -> Result<()> {
        let path = entry()?;
        if !enabled {
            match std::fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                _ => return Ok(()),
            }
        }
        let mut exec = program()?;
        for a in args {
            exec.push(' ');
            exec.push_str(a);
        }
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(
            &path,
            format!(
                "[Desktop Entry]\nType=Application\nName=oriel\nComment=Remote desktop\n\
                 Exec={exec}\nIcon=oriel\nX-GNOME-Autostart-enabled=true\n"
            ),
        )
        .with_context(|| format!("write {}", path.display()))
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;

    pub fn set(enabled: bool, args: &[&str]) -> Result<()> {
        let home = std::env::var_os("HOME").context("no home directory")?;
        let path = std::path::Path::new(&home).join("Library/LaunchAgents/org.oriel.oriel.plist");
        if !enabled {
            match std::fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                _ => return Ok(()),
            }
        }
        let mut arguments = format!("<string>{}</string>", program()?);
        for a in args {
            arguments.push_str(&format!("<string>{a}</string>"));
        }
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(
            &path,
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
                 <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
                 \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
                 <plist version=\"1.0\"><dict>\
                 <key>Label</key><string>org.oriel.oriel</string>\
                 <key>ProgramArguments</key><array>{arguments}</array>\
                 <key>RunAtLoad</key><true/>\
                 </dict></plist>\n"
            ),
        )
        .with_context(|| format!("write {}", path.display()))
    }
}

/// Android starts apps itself.
#[cfg(target_os = "android")]
mod imp {
    pub fn set(_enabled: bool, _args: &[&str]) -> anyhow::Result<()> {
        Ok(())
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use windows::{
        Win32::System::Registry::{HKEY_CURRENT_USER, REG_SZ, RegDeleteKeyValueW, RegSetKeyValueW},
        core::HSTRING,
    };

    const KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

    pub fn machine_wide() -> bool {
        use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
        unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                &HSTRING::from(KEY),
                &HSTRING::from("oriel"),
                RRF_RT_REG_SZ,
                None,
                None,
                None,
            )
            .is_ok()
        }
    }

    pub fn set(enabled: bool, args: &[&str]) -> Result<()> {
        let key = HSTRING::from(KEY);
        let name = HSTRING::from("oriel");
        unsafe {
            if !enabled {
                let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, &key, &name);
                return Ok(());
            }
            let mut command = format!("\"{}\"", program()?);
            for a in args {
                command.push(' ');
                command.push_str(a);
            }
            let wide: Vec<u16> = command.encode_utf16().chain([0]).collect();
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                &key,
                &name,
                REG_SZ.0,
                Some(wide.as_ptr() as *const _),
                (wide.len() * 2) as u32,
            )
            .ok()
            .context("write the Run registry key")?;
        }
        Ok(())
    }
}
