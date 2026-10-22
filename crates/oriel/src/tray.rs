//! Windows tray icon for a running host: status, pairing, log, quit. Also
//! restarts into a new build for `--update-from` development hosts.

use std::{
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use anyhow::Result;
use oriel_host::{HostControl, HostOptions};
use oriel_net::Paths;
use tray_icon::{
    Icon, TrayIconBuilder,
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
};
use windows::{
    Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, MB_ICONINFORMATION, MB_OK, MSG, MessageBoxW, MsgWaitForMultipleObjects,
        PM_REMOVE, PeekMessageW, QS_ALLINPUT, TranslateMessage,
    },
    core::HSTRING,
};

/// Set by the update watcher: restart into the new build.
pub static RESTART: AtomicBool = AtomicBool::new(false);

pub fn run_host(
    rt: tokio::runtime::Runtime,
    paths: Paths,
    options: HostOptions,
    log: Option<PathBuf>,
) -> Result<()> {
    let name = options.name.clone();
    let control = HostControl::new(options.pair_pin.clone());
    let task = rt.spawn(oriel_host::run_with(paths, options, control.clone()));

    let title = MenuItem::new(format!("oriel host — {name}"), false, None);
    let status = MenuItem::new("No client connected", false, None);
    let pin_item = MenuItem::new("", false, None);
    let pair = MenuItem::new("Allow pairing…", true, None);
    let open_log = MenuItem::new("Open log", log.is_some(), None);
    let quit = MenuItem::new("Quit", true, None);
    let menu = Menu::new();
    menu.append_items(&[
        &title,
        &status,
        &pin_item,
        &PredefinedMenuItem::separator(),
        &pair,
        &open_log,
        &PredefinedMenuItem::separator(),
        &quit,
    ])?;
    let (rgba, n) = oriel_client::launcher::icon_rgba();
    let tray = match TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(format!("oriel host — {name}"))
        .with_icon(Icon::from_rgba(rgba, n, n)?)
        .build()
    {
        Ok(t) => t,
        Err(e) => {
            // Keep hosting without the icon.
            tracing::error!("cannot show the tray icon: {e}");
            return match rt.block_on(task) {
                Ok(r) => r,
                Err(e) => Err(e.into()),
            };
        }
    };

    let mut last_update = Instant::now() - Duration::from_secs(1);
    let mut shown = String::new();
    loop {
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id == quit.id() {
                drop(tray);
                task.abort();
                return Ok(());
            } else if event.id == pair.id() {
                let pin = control.allow_pairing();
                let text = format!(
                    "Pairing PIN: {pin}\n\nIn the oriel launcher on the other computer, choose \
                     \"Pair with a host\", or run:\n  oriel pair <this-computer> --pin {pin}"
                );
                std::thread::spawn(move || unsafe {
                    MessageBoxW(
                        None,
                        &HSTRING::from(text),
                        &HSTRING::from("oriel"),
                        MB_OK | MB_ICONINFORMATION,
                    );
                });
            } else if event.id == open_log.id()
                && let Some(log) = &log
            {
                let _ = std::process::Command::new("notepad.exe").arg(log).spawn();
            }
        }
        if last_update.elapsed() > Duration::from_millis(500) {
            last_update = Instant::now();
            let s = control.status();
            let text = match (&s.client, &s.codec) {
                (Some(c), Some(codec)) => format!("Streaming to {c} ({codec})"),
                (Some(c), None) => format!("{c} connected"),
                _ => "No client connected".into(),
            };
            if text != shown {
                status.set_text(&text);
                let _ = tray.set_tooltip(Some(format!("oriel host — {name}\n{text}")));
                shown = text;
            }
            pin_item.set_text(
                control
                    .pairing_pin()
                    .map(|p| format!("Pairing PIN: {p}"))
                    .unwrap_or_default(),
            );
        }
        if task.is_finished() {
            drop(tray);
            return match rt.block_on(task) {
                Ok(r) => r,
                Err(e) => Err(e.into()),
            };
        }
        if RESTART.load(Ordering::Relaxed) {
            drop(tray);
            task.abort();
            crate::update::relaunch();
        }
        unsafe {
            MsgWaitForMultipleObjects(None, false, 200, QS_ALLINPUT);
        }
    }
}
