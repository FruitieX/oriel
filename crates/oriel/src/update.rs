//! Development hosts follow a build folder (`--update-from`): when a new
//! oriel.exe lands there, install it next to the running copy and restart.
//! A running executable cannot be overwritten but it can be renamed, so the
//! old files move aside first.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

fn mtime(p: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// Remove leftovers of previous updates.
pub fn clean_old() {
    let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(Path::to_path_buf))
    else {
        return;
    };
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            if e.path().extension().is_some_and(|x| x == "old") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

/// Watch `src` for a new build; `on_update` runs once it has settled.
pub fn watch(src: PathBuf, on_update: impl Fn() + Send + 'static) {
    let exe = src.join(if cfg!(windows) { "oriel.exe" } else { "oriel" });
    let initial = mtime(&exe);
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(2));
            let now = mtime(&exe);
            if now.is_some() && now != initial {
                // Let the copy finish.
                std::thread::sleep(Duration::from_secs(3));
                if let Err(e) = install(&src) {
                    tracing::error!("cannot install the new build: {e:#}");
                    continue;
                }
                tracing::info!("installed a new build from {}, restarting", src.display());
                on_update();
                return;
            }
        }
    });
}

fn install(src: &Path) -> anyhow::Result<()> {
    let dir = std::env::current_exe()?
        .parent()
        .ok_or_else(|| anyhow::anyhow!("no program directory"))?
        .to_path_buf();
    if dir == src.canonicalize().unwrap_or_else(|_| src.to_path_buf()) {
        anyhow::bail!("refusing to update in place from {}", src.display());
    }
    for entry in std::fs::read_dir(src)? {
        let path = entry?.path();
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !matches!(ext.as_str(), "exe" | "dll" | "bat") {
            continue;
        }
        let dest = dir.join(path.file_name().unwrap());
        if dest.exists() {
            let old = dest.with_extension(format!("{ext}.old"));
            let _ = std::fs::remove_file(&old);
            std::fs::rename(&dest, &old)?;
        }
        std::fs::copy(&path, &dest)?;
    }
    Ok(())
}

/// Start the installed program again with the same arguments and exit.
pub fn relaunch() -> ! {
    let exe = std::env::current_exe().expect("current exe");
    let args: Vec<String> = std::env::args().skip(1).collect();
    match std::process::Command::new(&exe).args(&args).spawn() {
        Ok(_) => std::process::exit(0),
        Err(e) => {
            tracing::error!("cannot restart {}: {e}", exe.display());
            std::process::exit(1)
        }
    }
}
