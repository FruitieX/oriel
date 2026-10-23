//! The start window: a tile per paired host (click to connect, right-click
//! for per-host options), sharing this computer, and dialogs for pairing and
//! settings. Connections run in their own viewer window; when one ends the
//! launcher comes back (and reconnects if the connection was lost).

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Result;
use oriel_net::{Paths, TrustStore};
use parking_lot::Mutex;
use winit::{
    application::ApplicationHandler,
    event::{StartCause, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    platform::run_on_demand::EventLoopExtRunOnDemand,
    window::{Icon, Window, WindowId},
};

use crate::{
    ViewerOptions,
    app::{self, ViewerExit},
    config::ClientConfig,
    connection::{self, Connected, Reach},
    render::Renderer,
    session::UiEvent,
    theme::{
        AMBER, BG, BLUE, CARD, CARD_HOVER, DEEP_BLUE, GREEN, MUTED, dialog_frame, paint_logo,
        primary_button, section_title, style,
    },
};

/// A client connected to this computer's host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedClient {
    pub id: usize,
    pub name: String,
    pub address: String,
    pub codec: Option<String>,
}

/// Lets the app start, stop and watch hosting this computer.
pub trait HostControl {
    fn start(&mut self) -> Result<()>;
    fn stop(&mut self);
    fn is_running(&self) -> bool;
    /// What would be shared, e.g. "Display 1 (2560×1440)".
    fn description(&self) -> String;
    fn host_name(&self) -> String;
    fn clients(&self) -> Vec<SharedClient>;
    fn kick(&self, id: usize);
    /// Accept pairing requests with a fresh PIN, which is returned.
    fn allow_pairing(&mut self) -> Option<String>;
    fn pairing_pin(&self) -> Option<String>;
    /// Stop accepting pairing requests.
    fn stop_pairing(&mut self);
    /// Whether hosting ends when the app quits (not for a service).
    fn stops_with_app(&self) -> bool {
        true
    }
}

pub struct GuiOptions {
    /// Connect to this host right away.
    pub connect: Option<String>,
    /// Use these settings for every connection instead of the saved ones
    /// (`oriel connect` with flags).
    pub viewer: Option<ViewerOptions>,
    /// Quit when the first viewer window is closed (`oriel connect`).
    pub quit_with_viewer: bool,
    /// Keep running in the tray when the window is closed (Windows).
    pub tray: bool,
    /// Start in the tray without opening the window.
    pub start_hidden: bool,
    /// Open the window minimized (where there is no tray).
    pub start_minimized: bool,
    /// Start sharing this computer right away.
    pub start_sharing: bool,
    /// Offered as "Open log" in the tray.
    pub log_path: Option<std::path::PathBuf>,
    /// Set from elsewhere to make the app quit (e.g. to restart into an
    /// update).
    pub quit_flag: Option<Arc<std::sync::atomic::AtomicBool>>,
}

// Not derivable on Windows, where the tray is on by default.
#[allow(clippy::derivable_impls)]
impl Default for GuiOptions {
    fn default() -> Self {
        Self {
            connect: None,
            viewer: None,
            quit_with_viewer: false,
            tray: cfg!(windows),
            start_hidden: false,
            start_minimized: false,
            start_sharing: false,
            log_path: None,
            quit_flag: None,
        }
    }
}

/// A small window icon: a frame with a lit pane.
pub fn icon() -> Option<Icon> {
    let (rgba, n) = icon_rgba();
    Icon::from_rgba(rgba, n, n).ok()
}

/// The icon as square RGBA pixels and its size.
pub fn icon_rgba() -> (Vec<u8>, u32) {
    let n = 32u32;
    let mut rgba = Vec::with_capacity((n * n * 4) as usize);
    for y in 0..n {
        for x in 0..n {
            let border = x < 2 || y < 2 || x >= n - 2 || y >= n - 2;
            let mullion = x == n / 2 || y == n / 2;
            let px = if border || mullion {
                [0x2a, 0x2f, 0x45, 255]
            } else if x < n / 2 && y < n / 2 {
                [0xff, 0xc4, 0x4d, 255]
            } else {
                [0x6c, 0x9c, 0xf0, 255]
            };
            rgba.extend_from_slice(&px);
        }
    }
    (rgba, n)
}

pub fn run(
    rt: tokio::runtime::Runtime,
    paths: Paths,
    host: Option<Box<dyn HostControl>>,
    options: GuiOptions,
) -> Result<()> {
    let mut builder = EventLoop::<UiEvent>::with_user_event();
    #[cfg(target_os = "android")]
    {
        use winit::platform::android::EventLoopBuilderExtAndroid;
        builder.with_android_app(crate::android::app().clone());
    }
    let mut event_loop = builder.build()?;
    #[cfg(windows)]
    let tray = if options.tray {
        match crate::tray::Tray::new(options.log_path.is_some()) {
            Ok(t) => Some(t),
            Err(e) => {
                tracing::error!("cannot show the tray icon: {e:#}");
                None
            }
        }
    } else {
        None
    };
    #[cfg(not(windows))]
    let tray: Option<()> = None;
    let config = ClientConfig::load(&paths).unwrap_or_default();
    let mut launcher = Launcher {
        has_tray: tray.is_some(),
        #[cfg(windows)]
        tray,
        window_wanted: !(options.start_hidden && options.connect.is_none()),
        start_minimized: options.start_minimized,
        log_path: options.log_path.clone(),
        quit_flag: options.quit_flag.clone(),
        quit: false,
        tray_updated: Instant::now(),
        config,
        paths,
        host,
        rt: Arc::new(rt),
        gfx: None,
        egui_ctx: egui::Context::default(),
        hosts: Vec::new(),
        reach: Default::default(),
        probe: None,
        next_probe: Instant::now(),
        connecting: None,
        pairing: None,
        pair_dialog: None,
        settings: None,
        status: String::new(),
        reconnect: None,
        result: None,
        opened: Instant::now(),
        screenshot_taken: false,
    };
    launcher.reload_hosts();
    // Debugging aid for screenshots.
    match std::env::var("ORIEL_DEBUG_DIALOG").as_deref() {
        Ok("pair") => launcher.pair_dialog = Some(PairDialog::default()),
        // "settings[-tab]" or "host-settings[-tab]" (for the first host).
        Ok(d) if d.starts_with("settings") || d.starts_with("host-settings") => {
            let host = d
                .starts_with("host")
                .then(|| launcher.hosts.first())
                .flatten();
            let mut dialog = SettingsDialog::new(host);
            if let Some((_, tab)) = d.rsplit_once('-')
                && let Some((t, _)) = crate::settings_ui::Panel::tabs(dialog.host.is_some())
                    .into_iter()
                    .find(|(_, l)| l.eq_ignore_ascii_case(tab))
            {
                dialog.panel.tab = t;
            }
            launcher.settings = Some(dialog);
        }
        _ => {}
    }
    if options.start_sharing
        && let Some(host) = &mut launcher.host
        && let Err(e) = host.start()
    {
        launcher.status = format!("Cannot share this computer: {e:#}");
    }
    if let Some(target) = &options.connect {
        launcher.connect(target.clone());
    }
    let mut first_viewer = true;

    loop {
        launcher.result = None;
        event_loop.run_app_on_demand(&mut launcher)?;
        let Some(LauncherResult::Connected(connected, target)) = launcher.result.take() else {
            if launcher.quit || !launcher.has_tray {
                break;
            }
            continue;
        };
        launcher.window_wanted = true;
        let viewer_options = match &options.viewer {
            Some(v) => ViewerOptions {
                target: target.clone(),
                ..v.clone()
            },
            None => ViewerOptions::from_config(
                &target,
                launcher.config.clone(),
                Some(launcher.paths.clone()),
            ),
        };
        let exit = app::run(&mut event_loop, &launcher.rt, connected, viewer_options)?;
        tracing::info!(?exit, "viewer ended");
        // The session saves the settings changed in its menu.
        launcher.config =
            ClientConfig::load(&launcher.paths).unwrap_or_else(|_| launcher.config.clone());
        let quit = options.quit_with_viewer && first_viewer && !launcher.has_tray;
        first_viewer = false;
        match exit {
            ViewerExit::Closed if quit => break,
            ViewerExit::Closed | ViewerExit::Back => {
                if quit {
                    break;
                }
                launcher.status.clear();
            }
            ViewerExit::Lost { reason, retry } => {
                launcher.status = format!("Disconnected from {target}: {reason}");
                if retry {
                    launcher.reconnect = Some(Reconnect {
                        target,
                        attempt: 1,
                        at: Instant::now() + Duration::from_secs(1),
                    });
                } else if quit {
                    break;
                }
            }
        }
    }
    if let Some(host) = &mut launcher.host
        && host.stops_with_app()
    {
        host.stop();
    }
    Ok(())
}

enum LauncherResult {
    Connected(Connected, String),
}

struct Reconnect {
    target: String,
    attempt: u32,
    at: Instant,
}

const DIM_SCREEN: egui::Color32 = egui::Color32::from_rgb(0x3a, 0x3f, 0x50);
/// The monitor glyph's bezel and stand, lighter than the tile behind them.
const BEZEL: egui::Color32 = egui::Color32::from_rgb(0x48, 0x51, 0x6e);
const BEZEL_HOVER: egui::Color32 = egui::Color32::from_rgb(0x5c, 0x68, 0x8c);

const PROBE_INTERVAL: Duration = Duration::from_secs(15);

type Pending<T> = Arc<Mutex<Option<Result<T>>>>;

/// A host's name and address as edited in its settings, stored when the
/// dialog closes.
struct HostEdit {
    /// The host's name before editing.
    original: String,
    name: String,
    address: String,
    error: Option<String>,
}

#[derive(Default)]
struct PairDialog {
    address: String,
    pin: String,
    name: String,
}

struct Launcher {
    #[cfg(windows)]
    tray: Option<crate::tray::Tray>,
    has_tray: bool,
    /// Whether the window should be open (it can be closed to the tray).
    window_wanted: bool,
    /// Minimize the window when it is first created.
    start_minimized: bool,
    #[cfg_attr(not(windows), allow(dead_code))]
    log_path: Option<std::path::PathBuf>,
    quit_flag: Option<Arc<std::sync::atomic::AtomicBool>>,
    quit: bool,
    #[cfg_attr(not(windows), allow(dead_code))]
    tray_updated: Instant,
    paths: Paths,
    config: ClientConfig,
    host: Option<Box<dyn HostControl>>,
    rt: Arc<tokio::runtime::Runtime>,
    gfx: Option<(Arc<Window>, Renderer, egui_winit::State)>,
    egui_ctx: egui::Context,
    hosts: Vec<oriel_net::TrustedPeer>,
    /// Whether each host (by name) answered the last check.
    reach: std::collections::HashMap<String, connection::Reach>,
    /// A running check of all hosts, and the names it covers.
    probe: Option<(Vec<String>, Pending<Vec<connection::Reach>>)>,
    next_probe: Instant,
    connecting: Option<(String, Pending<Connected>)>,
    pairing: Option<Pending<String>>,
    pair_dialog: Option<PairDialog>,
    /// The open settings dialog.
    settings: Option<SettingsDialog>,
    status: String,
    reconnect: Option<Reconnect>,
    result: Option<LauncherResult>,
    opened: Instant,
    screenshot_taken: bool,
}

enum Action {
    Connect(String),
    Forget(String),
    OpenPair,
    ClosePair,
    Pair,
    OpenSettings,
    OpenHostSettings(String),
    CloseSettings,
    /// The settings dialog changed the config.
    SettingsChanged,
    StartHosting,
    StopHosting,
    AllowPairing,
    StopPairing,
    Kick(usize),
    CancelReconnect,
}

/// The settings dialog: the defaults, or one host's settings.
struct SettingsDialog {
    host: Option<String>,
    panel: crate::settings_ui::Panel,
    edit: Option<HostEdit>,
}

impl SettingsDialog {
    fn new(host: Option<&oriel_net::TrustedPeer>) -> Self {
        Self {
            panel: crate::settings_ui::Panel::new(host.is_some()),
            host: host.map(|h| h.name.clone()),
            edit: host.map(|h| HostEdit {
                original: h.name.clone(),
                name: h.name.clone(),
                address: h.address.clone().unwrap_or_default(),
                error: None,
            }),
        }
    }
}

impl Launcher {
    fn reload_hosts(&mut self) {
        self.hosts = TrustStore::load(&self.paths.known_hosts())
            .map(|s| s.peers)
            .unwrap_or_default();
        self.next_probe = Instant::now();
    }

    /// Check which hosts are up, every [`PROBE_INTERVAL`] while the window
    /// is open.
    fn poll_probe(&mut self) {
        if let Some((names, pending)) = &self.probe {
            let Some(result) = pending.lock().take() else {
                return;
            };
            if let Ok(results) = result {
                self.reach = names.iter().cloned().zip(results).collect();
            }
            self.probe = None;
            self.next_probe = Instant::now() + PROBE_INTERVAL;
            if let Some((window, ..)) = &self.gfx {
                window.request_redraw();
            }
        }
        if self.gfx.is_none()
            || self.hosts.is_empty()
            || self.connecting.is_some()
            || Instant::now() < self.next_probe
        {
            return;
        }
        let hosts = self.hosts.clone();
        let pending: Pending<Vec<connection::Reach>> = Default::default();
        let out = pending.clone();
        let paths = self.paths.clone();
        self.rt.spawn(async move {
            let result = connection::probe(&paths, &hosts).await;
            *out.lock() = Some(result);
        });
        self.probe = Some((self.hosts.iter().map(|h| h.name.clone()).collect(), pending));
    }

    fn connect(&mut self, target: String) {
        let pending: Pending<Connected> = Default::default();
        let out = pending.clone();
        let paths = self.paths.clone();
        let t = target.clone();
        self.rt.spawn(async move {
            let result = connection::connect(&paths, &t).await;
            *out.lock() = Some(result);
        });
        self.status = format!("Connecting to {target}…");
        self.connecting = Some((target, pending));
    }

    fn save_config(&mut self) {
        if let Err(e) = self.config.save(&self.paths) {
            self.status = format!("Cannot save settings: {e:#}");
        }
    }

    fn apply(&mut self, action: Action) {
        match action {
            Action::Connect(target) => {
                self.reconnect = None;
                self.connect(target);
            }
            Action::Forget(name) => {
                let mut store = TrustStore::load(&self.paths.known_hosts()).unwrap_or_default();
                store.remove(&name);
                if let Err(e) = store.save(&self.paths.known_hosts()) {
                    self.status = format!("Cannot forget {name}: {e:#}");
                }
                if self.config.hosts.remove(&name).is_some() {
                    self.save_config();
                }
                self.reload_hosts();
            }
            Action::OpenPair => self.pair_dialog = Some(PairDialog::default()),
            Action::ClosePair => self.pair_dialog = None,
            Action::Pair => {
                let Some(d) = &self.pair_dialog else { return };
                let pending: Pending<String> = Default::default();
                let out = pending.clone();
                let paths = self.paths.clone();
                let (address, pin) = (d.address.trim().to_string(), d.pin.trim().to_string());
                let name = Some(d.name.trim().to_string()).filter(|n| !n.is_empty());
                self.rt.spawn(async move {
                    let result = connection::pair(&paths, &address, &pin, name.as_deref()).await;
                    *out.lock() = Some(result);
                });
                self.pairing = Some(pending);
            }
            Action::OpenSettings => self.settings = Some(SettingsDialog::new(None)),
            Action::OpenHostSettings(host) => {
                if let Some(h) = self.hosts.iter().find(|h| h.name == host) {
                    self.settings = Some(SettingsDialog::new(Some(h)));
                }
            }
            Action::CloseSettings => {
                // Store an edited name or address first; on a mistake the
                // dialog stays open and says why.
                if let Some(d) = self.settings.as_mut().and_then(|s| s.edit.as_mut())
                    && (d.name.trim() != d.original
                        || Some(d.address.trim())
                            != self
                                .hosts
                                .iter()
                                .find(|h| h.name == d.original)
                                .and_then(|h| h.address.as_deref()))
                {
                    match save_edit(&self.paths, &mut self.config, d) {
                        Ok(renamed) => {
                            if renamed {
                                self.save_config();
                            }
                            self.reach.clear();
                            self.reload_hosts();
                        }
                        Err(e) => {
                            d.error = Some(format!("{e:#}"));
                            return;
                        }
                    }
                }
                self.settings = None;
            }
            Action::SettingsChanged => {
                let saved = ClientConfig::load(&self.paths).unwrap_or_default();
                if (saved.autostart, saved.start_hidden)
                    != (self.config.autostart, self.config.start_hidden)
                    && let Err(e) =
                        crate::autostart::set(self.config.autostart, self.config.start_hidden)
                {
                    self.status = format!("Cannot change starting at login: {e:#}");
                    self.config.autostart = saved.autostart;
                }
                self.save_config();
            }
            Action::StartHosting => {
                if let Some(host) = &mut self.host {
                    match host.start() {
                        Ok(()) => self.set_sharing_preference(true),
                        Err(e) => self.status = format!("Cannot share this computer: {e:#}"),
                    }
                }
            }
            Action::StopHosting => {
                if let Some(host) = &mut self.host {
                    host.stop();
                }
                self.set_sharing_preference(false);
            }
            Action::AllowPairing => {
                if let Some(host) = &mut self.host {
                    host.allow_pairing();
                }
            }
            Action::StopPairing => {
                if let Some(host) = &mut self.host {
                    host.stop_pairing();
                }
            }
            Action::Kick(id) => {
                if let Some(host) = &self.host {
                    host.kick(id);
                }
            }
            Action::CancelReconnect => {
                self.reconnect = None;
                self.status.clear();
            }
        }
    }

    /// Remember whether to share on the next start.
    fn set_sharing_preference(&mut self, share: bool) {
        if self.config.share != share {
            self.config.share = share;
            let _ = self.config.save(&self.paths);
        }
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    fn open_window(&mut self, event_loop: &ActiveEventLoop) {
        self.window_wanted = true;
        if let Some((window, ..)) = &self.gfx {
            window.focus_window();
            window.request_redraw();
        } else {
            self.create_window(event_loop);
        }
    }

    fn create_window(&mut self, event_loop: &ActiveEventLoop) {
        let attrs = crate::app_window(Window::default_attributes())
            .with_title("oriel")
            .with_window_icon(icon())
            .with_inner_size(winit::dpi::LogicalSize::new(760.0, 600.0))
            .with_min_inner_size(winit::dpi::LogicalSize::new(420.0, 360.0));
        // Debugging aid: ORIEL_DEBUG_LAUNCHER_SIZE=WIDTHxHEIGHT.
        let attrs = match std::env::var("ORIEL_DEBUG_LAUNCHER_SIZE")
            .ok()
            .and_then(|s| crate::config::parse_resolution(&s))
        {
            Some((w, h)) => attrs
                .with_min_inner_size(winit::dpi::LogicalSize::new(w as f64, h as f64))
                .with_inner_size(winit::dpi::LogicalSize::new(w as f64, h as f64)),
            None => attrs,
        };
        let window = match event_loop.create_window(attrs) {
            Ok(w) => {
                if std::mem::take(&mut self.start_minimized) {
                    w.set_minimized(true);
                }
                Arc::new(w)
            }
            Err(e) => {
                tracing::error!("cannot create window: {e}");
                if !self.has_tray {
                    event_loop.exit();
                }
                return;
            }
        };
        let renderer = match Renderer::new(window.clone()) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("cannot initialise rendering: {e:#}");
                if !self.has_tray {
                    event_loop.exit();
                }
                return;
            }
        };
        // A fresh context: textures egui already sent belonged to the
        // previous window's renderer.
        self.egui_ctx = egui::Context::default();
        style(&self.egui_ctx);
        let egui = egui_winit::State::new(
            self.egui_ctx.clone(),
            egui::ViewportId::ROOT,
            &window,
            Some(window.scale_factor() as f32),
            None,
            Some(renderer.device.limits().max_texture_dimension_2d as usize),
        );
        window.request_redraw();
        self.gfx = Some((window, renderer, egui));
        self.opened = Instant::now();
    }

    /// Handle tray menu clicks and keep its status current.
    #[cfg(windows)]
    fn poll_tray(&mut self, event_loop: &ActiveEventLoop) {
        use crate::tray::TrayAction;
        let Some(tray) = &self.tray else { return };
        for action in tray.poll() {
            match action {
                TrayAction::Open => self.open_window(event_loop),
                TrayAction::Kick(id) => self.apply(Action::Kick(id)),
                TrayAction::TogglePairing => {
                    let pairing = self
                        .host
                        .as_ref()
                        .is_some_and(|h| h.pairing_pin().is_some());
                    self.apply(if pairing {
                        Action::StopPairing
                    } else {
                        Action::AllowPairing
                    });
                    // The PIN is shown in the window's sharing section.
                    self.open_window(event_loop);
                }
                TrayAction::ToggleSharing => {
                    let running = self.host.as_ref().is_some_and(|h| h.is_running());
                    self.apply(if running {
                        Action::StopHosting
                    } else {
                        Action::StartHosting
                    });
                }
                TrayAction::OpenLog => {
                    if let Some(log) = &self.log_path {
                        let _ = std::process::Command::new("notepad.exe").arg(log).spawn();
                    }
                }
                TrayAction::Quit => {
                    self.quit = true;
                    event_loop.exit();
                }
            }
        }
        if self.tray_updated.elapsed() > Duration::from_millis(500) {
            self.tray_updated = Instant::now();
            let host = self.host.as_deref();
            if let Some(tray) = &mut self.tray {
                tray.update(host);
            }
        }
    }

    /// Collect finished background work.
    fn poll(&mut self, event_loop: &ActiveEventLoop) {
        if let Some((target, pending)) = &self.connecting {
            let done = pending.lock().take();
            if let Some(result) = done {
                let target = target.clone();
                self.connecting = None;
                match result {
                    Ok(connected) => {
                        self.status.clear();
                        self.reconnect = None;
                        self.result = Some(LauncherResult::Connected(connected, target));
                        event_loop.exit();
                        return;
                    }
                    Err(e) => {
                        self.status = format!("Cannot connect to {target}: {e:#}");
                        self.next_probe = Instant::now();
                        if let Some(r) = &mut self.reconnect {
                            r.attempt += 1;
                            r.at = Instant::now()
                                + Duration::from_secs((r.attempt as u64 * 2).min(15));
                        }
                    }
                }
            }
        }
        if let Some(pending) = &self.pairing {
            let done = pending.lock().take();
            if let Some(result) = done {
                self.pairing = None;
                match result {
                    Ok(name) => {
                        self.status = format!("Paired with {name}");
                        self.pair_dialog = None;
                        self.reload_hosts();
                    }
                    Err(e) => self.status = format!("Pairing failed: {e:#}"),
                }
            }
        }
        self.poll_probe();
        if self.connecting.is_none()
            && let Some(r) = &self.reconnect
            && Instant::now() >= r.at
        {
            let target = r.target.clone();
            let attempt = r.attempt;
            self.connect(target.clone());
            self.status = format!("Reconnecting to {target} (attempt {attempt})…");
        }
    }

    fn ui(&mut self, root: &mut egui::Ui) -> Vec<Action> {
        let mut actions = Vec::new();
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BG).inner_margin(24))
            .show(root, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    self.header(ui, &mut actions);
                    self.status_banner(ui, &mut actions);
                    ui.add_space(16.0);
                    section_title(ui, "Computers");
                    self.host_tiles(ui, &mut actions);
                    if self.host.is_some() {
                        ui.add_space(20.0);
                        section_title(ui, "This computer");
                        self.sharing_card(ui, &mut actions);
                    }
                });
            });
        let ctx = root.ctx().clone();
        self.pair_modal(&ctx, &mut actions);
        self.settings_modal(&ctx, &mut actions);
        actions
    }

    fn header(&self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(30.0, 30.0), egui::Sense::hover());
            paint_logo(ui.painter(), rect);
            ui.label(egui::RichText::new("oriel").size(26.0).strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Settings").clicked() {
                    actions.push(Action::OpenSettings);
                }
                if ui.button("Pair a computer…").clicked() {
                    actions.push(Action::OpenPair);
                }
            });
        });
    }

    fn status_banner(&self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        if self.status.is_empty() {
            return;
        }
        ui.add_space(12.0);
        egui::Frame::new()
            .fill(CARD)
            .corner_radius(10)
            .inner_margin(egui::Margin::symmetric(14, 10))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if self.connecting.is_some() || self.pairing.is_some() {
                        ui.spinner();
                    }
                    ui.label(&self.status);
                    if self.reconnect.is_some() && ui.button("Cancel").clicked() {
                        actions.push(Action::CancelReconnect);
                    }
                });
            });
    }

    fn host_tiles(&self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        if self.hosts.is_empty() {
            egui::Frame::new()
                .fill(CARD)
                .corner_radius(12)
                .inner_margin(20)
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(egui::RichText::new("No computers yet").size(17.0).strong());
                    ui.label(
                        egui::RichText::new(
                            "On the computer you want to reach, open oriel and choose \
                             \"Allow pairing\" (or run `oriel host --pair`), then pair with it here.",
                        )
                        .color(MUTED),
                    );
                    ui.add_space(6.0);
                    if ui.add(primary_button("Pair a computer…")).clicked() {
                        actions.push(Action::OpenPair);
                    }
                });
            return;
        }
        let busy = self.connecting.is_some();
        let tile = egui::vec2(210.0, 160.0);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(14.0, 14.0);
            for h in &self.hosts {
                let connecting = self.connecting.as_ref().is_some_and(|(t, _)| *t == h.name);
                let reach = self.reach.get(&h.name).copied();
                // Unreachable hosts are dimmed but can still be tried.
                let dim = matches!(reach, Some(Reach::Offline)) && !connecting;
                let (rect, response) = ui.allocate_exact_size(tile, egui::Sense::click());
                // Also while over the "more" button on top of it.
                let hovered = (response.hovered() || ui.rect_contains_pointer(rect)) && !busy;
                let painter = ui.painter();
                painter.rect_filled(rect, 14, if hovered { CARD_HOVER } else { CARD });
                if hovered {
                    painter.rect_stroke(
                        rect,
                        14,
                        egui::Stroke::new(1.5, BLUE),
                        egui::StrokeKind::Inside,
                    );
                }
                // A monitor glyph.
                let screen = egui::Rect::from_center_size(
                    rect.center_top() + egui::vec2(0.0, 52.0),
                    egui::vec2(78.0, 48.0),
                );
                let bezel = if hovered { BEZEL_HOVER } else { BEZEL };
                painter.rect_filled(screen, 6, bezel);
                painter.rect_filled(
                    screen.shrink(5.0),
                    3,
                    match (hovered, dim) {
                        (true, _) => BLUE,
                        (false, true) => DIM_SCREEN,
                        (false, false) => DEEP_BLUE,
                    },
                );
                // Reachability in the top left corner.
                let (dot, note) = match reach {
                    Some(Reach::Online) => (Some(GREEN), None),
                    Some(Reach::Offline) => (Some(MUTED), Some("Offline")),
                    Some(Reach::Changed) => (Some(AMBER), Some("Identity changed")),
                    None => (None, None),
                };
                if let Some(color) = dot {
                    painter.circle_filled(rect.left_top() + egui::vec2(20.0, 22.0), 5.0, color);
                }
                let stand = egui::Rect::from_center_size(
                    screen.center_bottom() + egui::vec2(0.0, 8.0),
                    egui::vec2(30.0, 4.0),
                );
                painter.rect_filled(stand, 2, bezel);
                painter.text(
                    rect.center_top() + egui::vec2(0.0, 108.0),
                    egui::Align2::CENTER_CENTER,
                    &h.name,
                    egui::FontId::proportional(18.0),
                    if dim { MUTED } else { egui::Color32::WHITE },
                );
                let address = h.address.as_deref().unwrap_or("");
                painter.text(
                    rect.center_top() + egui::vec2(0.0, 132.0),
                    egui::Align2::CENTER_CENTER,
                    match note {
                        Some(note) if !address.is_empty() => format!("{address} · {note}"),
                        Some(note) => note.to_string(),
                        None => address.to_string(),
                    },
                    egui::FontId::proportional(12.5),
                    if reach == Some(Reach::Changed) {
                        AMBER
                    } else {
                        MUTED
                    },
                );
                if connecting {
                    let spin =
                        egui::Rect::from_center_size(screen.center(), egui::vec2(22.0, 22.0));
                    ui.put(spin, egui::Spinner::new().color(egui::Color32::WHITE));
                }
                // A "more" button in the corner opens the same menu as a
                // right click.
                let more_rect = egui::Rect::from_min_size(
                    rect.right_top() + egui::vec2(-36.0, 8.0),
                    egui::vec2(28.0, 28.0),
                );
                let more = crate::theme::more_button(
                    ui,
                    more_rect,
                    egui::Id::unique("host-more").with(&h.name),
                )
                .on_hover_text("More");
                let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
                let response = match reach {
                    Some(Reach::Offline) => response
                        .on_hover_text("Not answering right now. Click to try connecting anyway."),
                    Some(Reach::Changed) => response.on_hover_text(
                        "Something answers at this address, but not the computer you paired \
                         with. If oriel was reinstalled there, pair again.",
                    ),
                    _ => response,
                };
                if response.clicked() && !busy && !more.hovered() {
                    actions.push(Action::Connect(h.name.clone()));
                }
                let menu = |ui: &mut egui::Ui, actions: &mut Vec<Action>| {
                    if ui.button("Connect").clicked() {
                        actions.push(Action::Connect(h.name.clone()));
                        ui.close();
                    }
                    if ui.button("Settings…").clicked() {
                        actions.push(Action::OpenHostSettings(h.name.clone()));
                        ui.close();
                    }
                    ui.separator();
                    if ui.button(format!("Forget {}", h.name)).clicked() {
                        actions.push(Action::Forget(h.name.clone()));
                        ui.close();
                    }
                };
                egui::Popup::menu(&more).show(|ui| menu(ui, actions));
                response.context_menu(|ui| menu(ui, actions));
            }
        });
    }

    fn sharing_card(&self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        let Some(host) = &self.host else { return };
        egui::Frame::new()
            .fill(CARD)
            .corner_radius(12)
            .inner_margin(18)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                let running = host.is_running();
                ui.horizontal(|ui| {
                    let (dot, text) = if running {
                        (GREEN, format!("Sharing as {}", host.host_name()))
                    } else {
                        (MUTED, "Not sharing".to_string())
                    };
                    let (r, _) =
                        ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                    ui.painter().circle_filled(r.center(), 5.0, dot);
                    ui.label(egui::RichText::new(text).size(17.0).strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if running {
                            if ui.button("Stop sharing").clicked() {
                                actions.push(Action::StopHosting);
                            }
                        } else if ui.add(primary_button("Share")).clicked() {
                            actions.push(Action::StartHosting);
                        }
                    });
                });
                ui.label(egui::RichText::new(host.description()).color(MUTED));
                if !running {
                    return;
                }
                ui.add_space(6.0);
                match host.pairing_pin() {
                    Some(pin) => {
                        ui.horizontal(|ui| {
                            ui.label("Pairing PIN");
                            ui.label(
                                egui::RichText::new(pin)
                                    .size(26.0)
                                    .strong()
                                    .monospace()
                                    .color(AMBER),
                            );
                            if ui.button("Stop pairing").clicked() {
                                actions.push(Action::StopPairing);
                            }
                        });
                        ui.label(
                            egui::RichText::new(
                                "Enter it under \"Pair a computer…\" on the other device.",
                            )
                            .color(MUTED),
                        );
                    }
                    None => {
                        if ui.button("Allow pairing a new device").clicked() {
                            actions.push(Action::AllowPairing);
                        }
                    }
                }
                let clients = host.clients();
                ui.add_space(6.0);
                if clients.is_empty() {
                    ui.label(egui::RichText::new("Nobody is connected.").color(MUTED));
                }
                for c in clients {
                    ui.horizontal(|ui| {
                        let codec = c
                            .codec
                            .as_deref()
                            .map(|c| format!(" · {c}"))
                            .unwrap_or_default();
                        ui.label(format!("{} ({}){codec}", c.name, c.address));
                        if ui.small_button("Disconnect").clicked() {
                            actions.push(Action::Kick(c.id));
                        }
                    });
                }
            });
    }

    fn pair_modal(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        let busy = self.pairing.is_some();
        let Some(d) = &mut self.pair_dialog else {
            return;
        };
        let modal = egui::Modal::new(egui::Id::unique("pair"))
            .frame(dialog_frame())
            .show(ctx, |ui| {
                ui.set_width(380.0);
                ui.heading("Pair a computer");
                ui.label(
                    egui::RichText::new(
                        "On the other computer, open oriel and choose \"Allow pairing\" \
                     (or run `oriel host --pair`). Then enter its address and the PIN it shows.",
                    )
                    .color(MUTED),
                );
                ui.add_space(8.0);
                egui::Grid::new("pair-fields")
                    .num_columns(2)
                    .spacing([12.0, 10.0])
                    .show(ui, |ui| {
                        ui.label("Address");
                        ui.add(
                            egui::TextEdit::singleline(&mut d.address)
                                .hint_text("192.168.1.10 or name"),
                        );
                        ui.end_row();
                        ui.label("PIN");
                        ui.add(egui::TextEdit::singleline(&mut d.pin).hint_text("6 digits"));
                        ui.end_row();
                        ui.label("Name");
                        ui.add(egui::TextEdit::singleline(&mut d.name).hint_text("optional"));
                        ui.end_row();
                    });
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    let ready = !d.address.trim().is_empty() && d.pin.trim().len() == 6 && !busy;
                    if ui.add_enabled(ready, primary_button("Pair")).clicked() {
                        actions.push(Action::Pair);
                    }
                    if ui.button("Cancel").clicked() {
                        actions.push(Action::ClosePair);
                    }
                    if busy {
                        ui.spinner();
                    }
                });
            });
        if modal.should_close() && !busy {
            actions.push(Action::ClosePair);
        }
    }

    fn settings_modal(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        let Some(dialog) = &mut self.settings else {
            return;
        };
        let screen = ctx.content_rect();
        // One size on every tab, within the window; what does not fit
        // scrolls.
        let width = (screen.width() - 92.0).clamp(240.0, 560.0);
        // Narrow dialogs wrap their tabs and subtitle onto more lines.
        let chrome = if width < 440.0 { 290.0 } else { 230.0 }
            + if dialog.edit.is_some() { 96.0 } else { 0.0 };
        let height = (screen.height() - chrome).clamp(60.0, 380.0);
        let config = &mut self.config;
        let modal = egui::Modal::new(egui::Id::unique("settings"))
            .frame(crate::theme::dialog_frame())
            .show(ctx, |ui| {
                ui.set_width(width);
                let (title, subtitle) = match &dialog.host {
                    Some(h) => (
                        format!("{h} settings"),
                        "Used when connected to this computer; Reset uses the default.",
                    ),
                    None => (
                        "Settings".to_string(),
                        "Defaults for every computer; each can override them.",
                    ),
                };
                if crate::theme::title_bar(ui, &title, Some(subtitle)) {
                    actions.push(Action::CloseSettings);
                }
                ui.add_space(10.0);
                if let Some(d) = &mut dialog.edit {
                    egui::Grid::new("host-identity")
                        .num_columns(2)
                        .spacing([12.0, 8.0])
                        .show(ui, |ui| {
                            let field = (width - 110.0).max(120.0);
                            ui.label("Name");
                            ui.add(egui::TextEdit::singleline(&mut d.name).desired_width(field));
                            ui.end_row();
                            ui.label("Address");
                            ui.add(
                                egui::TextEdit::singleline(&mut d.address)
                                    .hint_text("IP address or host name, :port if not 48600")
                                    .desired_width(field),
                            );
                            ui.end_row();
                        });
                    if let Some(error) = &d.error {
                        ui.label(egui::RichText::new(error).color(crate::theme::RED));
                    }
                    ui.add_space(6.0);
                    ui.separator();
                    ui.add_space(6.0);
                }
                let outcome = dialog.panel.show(
                    ui,
                    config,
                    dialog.host.as_deref(),
                    None,
                    egui::vec2(width, height),
                );
                if outcome.changed {
                    actions.push(Action::SettingsChanged);
                }
                ui.add_space(12.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add(primary_button("Done")).clicked() {
                        actions.push(Action::CloseSettings);
                    }
                });
            });
        if modal.should_close() {
            actions.push(Action::CloseSettings);
        }
    }
}

impl ApplicationHandler<UiEvent> for Launcher {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gfx.is_none() && self.window_wanted {
            self.create_window(event_loop);
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some((window, renderer, egui)) = &mut self.gfx else {
            return;
        };
        // egui-winit asks for a repaint on every event, including redraws
        // themselves; following that blindly redraws in a busy loop.
        if !matches!(event, WindowEvent::RedrawRequested)
            && egui.on_window_event(window, &event).repaint
        {
            window.request_redraw();
        }
        match event {
            WindowEvent::CloseRequested => {
                if self.has_tray {
                    // Keep running in the tray.
                    self.gfx = None;
                    self.window_wanted = false;
                } else {
                    event_loop.exit();
                }
            }
            WindowEvent::Resized(size) => {
                renderer.resize(size.width, size.height);
                window.request_redraw();
            }
            WindowEvent::RedrawRequested => {
                let input = egui.take_egui_input(window);
                let ctx = self.egui_ctx.clone();
                let mut actions = Vec::new();
                let output = ctx.run_ui(input, |ui| actions = self.ui(ui));
                let repaint_now = output
                    .viewport_output
                    .get(&egui::ViewportId::ROOT)
                    .is_some_and(|v| v.repaint_delay.is_zero());
                let Some((window, renderer, egui)) = &mut self.gfx else {
                    return;
                };
                egui.handle_platform_output(window, output.platform_output.clone());
                if let Err(e) = renderer.render(None, &ctx, output) {
                    tracing::error!("render failed: {e:#}");
                }
                let repaint = !actions.is_empty() || repaint_now;
                for a in actions {
                    self.apply(a);
                }
                if repaint && let Some((window, ..)) = &self.gfx {
                    window.request_redraw();
                }
            }
            _ => {}
        }
    }

    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: StartCause) {
        if matches!(cause, StartCause::ResumeTimeReached { .. }) {
            self.poll(event_loop);
            if let Some((window, ..)) = &self.gfx {
                window.request_redraw();
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Debugging aid: ORIEL_DEBUG_SCREENSHOT=path saves the launcher and
        // quits (after ORIEL_DEBUG_SCREENSHOT_AFTER seconds, default 1).
        if let Some(path) = std::env::var_os("ORIEL_DEBUG_SCREENSHOT")
            && self.opened.elapsed()
                > Duration::from_secs_f64(
                    std::env::var("ORIEL_DEBUG_SCREENSHOT_AFTER")
                        .ok()
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(1.0),
                )
            && let Some((window, renderer, _)) = &mut self.gfx
        {
            if renderer.screenshot.is_none() && !self.screenshot_taken {
                renderer.screenshot = Some(path.into());
                self.screenshot_taken = true;
                window.request_redraw();
            } else if self.screenshot_taken && renderer.screenshot.is_none() {
                event_loop.exit();
                return;
            }
        }
        self.poll(event_loop);
        #[cfg(windows)]
        self.poll_tray(event_loop);
        if self
            .quit_flag
            .as_ref()
            .is_some_and(|f| f.load(std::sync::atomic::Ordering::Relaxed))
        {
            self.quit = true;
            event_loop.exit();
            return;
        }
        // The tray and the sharing status need polling.
        let sharing = self.host.as_ref().is_some_and(|h| h.is_running());
        let waiting = self.has_tray
            || (sharing && self.gfx.is_some())
            || self.connecting.is_some()
            || self.pairing.is_some()
            || self.probe.is_some()
            || self.reconnect.is_some()
            || std::env::var_os("ORIEL_DEBUG_SCREENSHOT").is_some();
        event_loop.set_control_flow(if waiting {
            ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(100))
        } else if self.gfx.is_some() && !self.hosts.is_empty() {
            ControlFlow::WaitUntil(self.next_probe)
        } else {
            ControlFlow::Wait
        });
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        self.gfx = None;
    }
}

/// Store an edited name and address. Returns whether the host was renamed
/// (its settings in `config` move along and need saving).
fn save_edit(paths: &Paths, config: &mut ClientConfig, d: &HostEdit) -> Result<bool> {
    let (name, address) = (d.name.trim(), d.address.trim());
    let mut store = TrustStore::load(&paths.known_hosts())?;
    if store
        .peers
        .iter()
        .any(|p| p.name.eq_ignore_ascii_case(name) && p.name != d.original)
    {
        anyhow::bail!("another computer is already called {name}");
    }
    let Some(peer) = store.peers.iter_mut().find(|p| p.name == d.original) else {
        anyhow::bail!("{} is no longer paired", d.original);
    };
    peer.name = name.to_string();
    peer.address = Some(address.to_string());
    store.save(&paths.known_hosts())?;
    let renamed = name != d.original;
    if renamed && let Some(prefs) = config.hosts.remove(&d.original) {
        config.hosts.insert(name.to_string(), prefs);
    }
    Ok(renamed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oriel_net::TrustedPeer;

    #[test]
    fn edit_renames_and_moves_settings() {
        let dir = std::env::temp_dir().join(format!("oriel-edit-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let paths = Paths { config_dir: dir };
        let fp = |b| oriel_net::Fingerprint([b; 32]);
        let mut store = TrustStore::default();
        for (name, b) in [("alpha", 1), ("beta", 2)] {
            store.upsert(TrustedPeer {
                name: name.into(),
                fingerprint: fp(b),
                address: Some("10.0.0.1".into()),
            });
        }
        store.save(&paths.known_hosts()).unwrap();
        let mut config = ClientConfig::default();
        config.host_mut("alpha").show_stats = Some(true);

        let mut d = HostEdit {
            original: "alpha".into(),
            name: "Beta".into(),
            address: "10.0.0.9".into(),
            error: None,
        };
        assert!(
            save_edit(&paths, &mut config, &d).is_err(),
            "names are unique"
        );

        d.name = " gamma ".into();
        assert!(save_edit(&paths, &mut config, &d).unwrap());
        let store = TrustStore::load(&paths.known_hosts()).unwrap();
        let gamma = store.by_fingerprint(&fp(1)).unwrap();
        assert_eq!(gamma.name, "gamma");
        assert_eq!(gamma.address.as_deref(), Some("10.0.0.9"));
        assert_eq!(config.hosts["gamma"].show_stats, Some(true));
        assert!(!config.hosts.contains_key("alpha"));
        let _ = std::fs::remove_dir_all(&paths.config_dir);
    }
}
