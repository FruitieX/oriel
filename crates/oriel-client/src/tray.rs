//! The tray icon (Windows): sharing status, connected clients, pairing and
//! a way back to the window. Its menu events are polled by the launcher's
//! event loop, which also pumps the Win32 messages the icon needs.

use anyhow::Result;
use tray_icon::{
    Icon, TrayIcon, TrayIconBuilder,
    menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu},
};

use crate::launcher::{HostControl, SharedClient, icon_rgba};

pub enum TrayAction {
    Open,
    Kick(usize),
    /// Allow pairing, or stop allowing it.
    TogglePairing,
    ToggleSharing,
    OpenLog,
    Quit,
}

pub struct Tray {
    icon: TrayIcon,
    menu: Menu,
    status: MenuItem,
    /// Shown only while pairing is allowed (an empty item would show as a
    /// blank row).
    pin_shown: bool,
    pin: MenuItem,
    clients_menu: Submenu,
    client_items: Vec<(MenuItem, usize)>,
    none_item: MenuItem,
    shown_clients: Vec<SharedClient>,
    open: MenuItem,
    share: MenuItem,
    pair: MenuItem,
    log: MenuItem,
    quit: MenuItem,
    tooltip: String,
}

impl Tray {
    pub fn new(has_log: bool) -> Result<Self> {
        let open = MenuItem::new("Open oriel", true, None);
        let status = MenuItem::new("Not sharing", false, None);
        let pin = MenuItem::new("", false, None);
        let clients_menu = Submenu::new("Connected clients", true);
        let none_item = MenuItem::new("None", false, None);
        clients_menu.append(&none_item)?;
        let share = MenuItem::new("Share this computer", true, None);
        let pair = MenuItem::new("Allow pairing…", true, None);
        let log = MenuItem::new("Open log", has_log, None);
        let quit = MenuItem::new("Quit", true, None);
        let menu = Menu::new();
        menu.append_items(&[
            &open,
            &PredefinedMenuItem::separator(),
            &status,
            &clients_menu,
            &PredefinedMenuItem::separator(),
            &share,
            &pair,
            &log,
            &PredefinedMenuItem::separator(),
            &quit,
        ])?;
        let (rgba, n) = icon_rgba();
        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu.clone()))
            .with_tooltip("oriel")
            .with_icon(Icon::from_rgba(rgba, n, n)?)
            .build()?;
        Ok(Self {
            icon,
            menu,
            status,
            pin_shown: false,
            pin,
            clients_menu,
            client_items: Vec::new(),
            none_item,
            shown_clients: Vec::new(),
            open,
            share,
            pair,
            log,
            quit,
            tooltip: String::new(),
        })
    }

    pub fn poll(&self) -> Vec<TrayAction> {
        let mut out = Vec::new();
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            let id: &MenuId = &event.id;
            if id == self.open.id() {
                out.push(TrayAction::Open);
            } else if id == self.share.id() {
                out.push(TrayAction::ToggleSharing);
            } else if id == self.pair.id() {
                out.push(TrayAction::TogglePairing);
            } else if id == self.log.id() {
                out.push(TrayAction::OpenLog);
            } else if id == self.quit.id() {
                out.push(TrayAction::Quit);
            } else if let Some((_, client)) =
                self.client_items.iter().find(|(item, _)| item.id() == id)
            {
                out.push(TrayAction::Kick(*client));
            }
        }
        out
    }

    pub fn update(&mut self, host: Option<&dyn HostControl>) {
        let (sharing, clients, pin, name) = match host {
            Some(h) => (h.is_running(), h.clients(), h.pairing_pin(), h.host_name()),
            None => (false, Vec::new(), None, String::new()),
        };
        let status = if !sharing {
            "Not sharing".to_string()
        } else if clients.is_empty() {
            format!("Sharing {name}, nobody connected")
        } else {
            format!("Sharing {name} with {}", names(&clients))
        };
        self.status.set_text(&status);
        self.share.set_text(if sharing {
            "Stop sharing"
        } else {
            "Share this computer"
        });
        self.pair.set_enabled(sharing);
        self.pair.set_text(if pin.is_some() {
            "Stop pairing"
        } else {
            "Allow pairing…"
        });
        if let Some(p) = &pin {
            self.pin.set_text(format!("Pairing PIN: {p}"));
        }
        if pin.is_some() != self.pin_shown {
            // Right below the status line.
            let _ = if pin.is_some() {
                self.menu.insert(&self.pin, 3)
            } else {
                self.menu.remove(&self.pin)
            };
            self.pin_shown = pin.is_some();
        }
        if clients != self.shown_clients {
            for (item, _) in self.client_items.drain(..) {
                let _ = self.clients_menu.remove(&item);
            }
            let _ = self.clients_menu.remove(&self.none_item);
            if clients.is_empty() {
                let _ = self.clients_menu.append(&self.none_item);
            }
            for c in &clients {
                let codec = c
                    .codec
                    .as_deref()
                    .map(|c| format!(", {c}"))
                    .unwrap_or_default();
                let item = MenuItem::new(
                    format!("Disconnect {} ({}{codec})", c.name, c.address),
                    true,
                    None,
                );
                let _ = self.clients_menu.append(&item);
                self.client_items.push((item, c.id));
            }
            self.shown_clients = clients;
        }
        let tooltip = format!("oriel\n{status}");
        if tooltip != self.tooltip {
            let _ = self.icon.set_tooltip(Some(&tooltip));
            self.tooltip = tooltip;
        }
    }
}

fn names(clients: &[SharedClient]) -> String {
    clients
        .iter()
        .map(|c| c.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}
