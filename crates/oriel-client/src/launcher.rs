//! The start window: paired hosts, pairing, settings and sharing this
//! computer. Connections run in their own viewer window; when one ends the
//! launcher comes back (and reconnects if the connection was lost).

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Result;
use oriel_net::{Paths, TrustStore};
use oriel_proto::StreamSettings;
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
    connection::{self, Connected},
    keymap::Shortcut,
    render::Renderer,
    session::UiEvent,
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
    let mut event_loop = EventLoop::<UiEvent>::with_user_event().build()?;
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
    let mut launcher = Launcher {
        has_tray: tray.is_some(),
        #[cfg(windows)]
        tray,
        window_wanted: !(options.start_hidden && options.connect.is_none()),
        log_path: options.log_path.clone(),
        quit_flag: options.quit_flag.clone(),
        quit: false,
        tray_updated: Instant::now(),
        config: ClientConfig::load(&paths).unwrap_or_default(),
        paths,
        host,
        rt: Arc::new(rt),
        gfx: None,
        egui_ctx: egui::Context::default(),
        hosts: Vec::new(),
        connecting: None,
        pairing: None,
        pair_address: String::new(),
        pair_pin: String::new(),
        pair_name: String::new(),
        status: String::new(),
        reconnect: None,
        result: None,
        shortcut_text: String::new(),
        opened: Instant::now(),
        screenshot_taken: false,
    };
    launcher.egui_ctx.set_visuals(egui::Visuals::dark());
    launcher.shortcut_text = launcher.config.menu_shortcut.clone();
    launcher.reload_hosts();
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
            None => launcher.viewer_options(&target)?,
        };
        let exit = app::run(&mut event_loop, &launcher.rt, connected, viewer_options)?;
        tracing::info!(?exit, "viewer ended");
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
    if let Some(host) = &mut launcher.host {
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

type Pending<T> = Arc<Mutex<Option<Result<T>>>>;

struct Launcher {
    #[cfg(windows)]
    tray: Option<crate::tray::Tray>,
    has_tray: bool,
    /// Whether the window should be open (it can be closed to the tray).
    window_wanted: bool,
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
    connecting: Option<(String, Pending<Connected>)>,
    pairing: Option<Pending<String>>,
    pair_address: String,
    pair_pin: String,
    pair_name: String,
    status: String,
    reconnect: Option<Reconnect>,
    result: Option<LauncherResult>,
    shortcut_text: String,
    opened: Instant,
    screenshot_taken: bool,
}

enum Action {
    Connect(String),
    Forget(String),
    Pair,
    StartHosting,
    StopHosting,
    AllowPairing,
    Kick(usize),
    CancelReconnect,
    SaveSettings,
}

impl Launcher {
    fn reload_hosts(&mut self) {
        self.hosts = TrustStore::load(&self.paths.known_hosts())
            .map(|s| s.peers)
            .unwrap_or_default();
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

    fn viewer_options(&self, target: &str) -> Result<ViewerOptions> {
        let c = &self.config;
        let (codecs, auto_codec) = c.codecs()?;
        Ok(ViewerOptions {
            target: target.to_string(),
            settings: StreamSettings {
                display: None,
                codecs,
                bitrate_kbps: c.bitrate_kbps,
                max_fps: c.max_fps,
                audio: c.audio,
                auto_codec,
                pyrowave_kbps: c.pyrowave_kbps,
            },
            hardware_decode: true,
            menu_shortcut: Shortcut::parse(&c.menu_shortcut)
                .unwrap_or_else(|| Shortcut::parse("Ctrl+Alt+Shift+M").unwrap()),
            mouse_mode: c.mouse_mode()?,
            fullscreen: c.fullscreen,
            show_stats: c.show_stats,
            screenshot: None,
        })
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
                self.reload_hosts();
            }
            Action::Pair => {
                let pending: Pending<String> = Default::default();
                let out = pending.clone();
                let paths = self.paths.clone();
                let (address, pin) = (
                    self.pair_address.trim().to_string(),
                    self.pair_pin.trim().to_string(),
                );
                let name = Some(self.pair_name.trim().to_string()).filter(|n| !n.is_empty());
                self.rt.spawn(async move {
                    let result = connection::pair(&paths, &address, &pin, name.as_deref()).await;
                    *out.lock() = Some(result);
                });
                self.status = "Pairing…".into();
                self.pairing = Some(pending);
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
            Action::Kick(id) => {
                if let Some(host) = &self.host {
                    host.kick(id);
                }
            }
            Action::CancelReconnect => {
                self.reconnect = None;
                self.status.clear();
            }
            Action::SaveSettings => {
                match Shortcut::parse(&self.shortcut_text) {
                    Some(s) => self.config.menu_shortcut = s.to_string(),
                    None => {
                        self.status =
                            format!("Cannot understand the shortcut {:?}", self.shortcut_text);
                        return;
                    }
                }
                if let Err(e) = self.config.save(&self.paths) {
                    self.status = format!("Cannot save settings: {e:#}");
                }
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
        let attrs = Window::default_attributes()
            .with_title("oriel")
            .with_window_icon(icon())
            .with_inner_size(winit::dpi::LogicalSize::new(620.0, 680.0));
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
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
                TrayAction::AllowPairing => {
                    self.apply(Action::AllowPairing);
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
                        self.pair_pin.clear();
                        self.reload_hosts();
                    }
                    Err(e) => self.status = format!("Pairing failed: {e:#}"),
                }
            }
        }
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
        let busy = self.connecting.is_some() || self.pairing.is_some();
        egui::CentralPanel::default().show(root, |ui| {
            ui.heading("oriel");
            if !self.status.is_empty() {
                ui.horizontal(|ui| {
                    if busy {
                        ui.spinner();
                    }
                    ui.label(&self.status);
                    if self.reconnect.is_some() && ui.button("Cancel").clicked() {
                        actions.push(Action::CancelReconnect);
                    }
                });
            }
            ui.add_space(8.0);

            ui.label(egui::RichText::new("Hosts").strong());
            if self.hosts.is_empty() {
                ui.label("No paired hosts yet. Pair one below.");
            }
            egui::Grid::new("hosts").num_columns(4).spacing([12.0, 6.0]).show(ui, |ui| {
                for h in &self.hosts {
                    ui.label(&h.name);
                    ui.label(h.address.as_deref().unwrap_or(""));
                    if ui.add_enabled(!busy, egui::Button::new("Connect")).clicked() {
                        actions.push(Action::Connect(h.name.clone()));
                    }
                    if ui.small_button("Forget").clicked() {
                        actions.push(Action::Forget(h.name.clone()));
                    }
                    ui.end_row();
                }
            });
            ui.add_space(12.0);

            ui.label(egui::RichText::new("Pair with a host").strong());
            ui.label("Start the host with `oriel host --pair` (or `oriel service pair`), then enter its address and PIN.");
            egui::Grid::new("pair").num_columns(2).show(ui, |ui| {
                ui.label("Address");
                ui.text_edit_singleline(&mut self.pair_address);
                ui.end_row();
                ui.label("PIN");
                ui.text_edit_singleline(&mut self.pair_pin);
                ui.end_row();
                ui.label("Name (optional)");
                ui.text_edit_singleline(&mut self.pair_name);
                ui.end_row();
            });
            let ready = !self.pair_address.trim().is_empty() && self.pair_pin.trim().len() == 6;
            if ui.add_enabled(ready && !busy, egui::Button::new("Pair")).clicked() {
                actions.push(Action::Pair);
            }
            ui.add_space(12.0);

            if let Some(host) = &self.host {
                ui.label(egui::RichText::new("Share this computer").strong());
                ui.label(host.description());
                if host.is_running() {
                    ui.horizontal(|ui| {
                        ui.label(format!("Sharing as {}.", host.host_name()));
                        if ui.button("Stop sharing").clicked() {
                            actions.push(Action::StopHosting);
                        }
                    });
                    match host.pairing_pin() {
                        Some(pin) => {
                            ui.label(
                                egui::RichText::new(format!("Pairing PIN: {pin}"))
                                    .size(20.0)
                                    .strong(),
                            );
                            ui.label("Enter it under \"Pair with a host\" on the other computer.");
                        }
                        None => {
                            if ui.button("Allow pairing a new device").clicked() {
                                actions.push(Action::AllowPairing);
                            }
                        }
                    }
                    let clients = host.clients();
                    if clients.is_empty() {
                        ui.label("Nobody is connected.");
                    }
                    for c in clients {
                        ui.horizontal(|ui| {
                            let codec = c.codec.as_deref().map(|c| format!(" · {c}")).unwrap_or_default();
                            ui.label(format!("{} ({}){codec}", c.name, c.address));
                            if ui.small_button("Disconnect").clicked() {
                                actions.push(Action::Kick(c.id));
                            }
                        });
                    }
                } else if ui.button("Share").clicked() {
                    actions.push(Action::StartHosting);
                }
                ui.add_space(12.0);
            }

            egui::CollapsingHeader::new("Settings").show(ui, |ui| {
                let c = &mut self.config;
                egui::Grid::new("settings").num_columns(2).show(ui, |ui| {
                    ui.label("Codec");
                    let current = c.codecs.first().cloned().unwrap_or_default();
                    egui::ComboBox::from_id_salt("codec")
                        .selected_text(codec_label(&current))
                        .show_ui(ui, |ui| {
                            for (value, list) in [
                                ("auto", vec!["auto"]),
                                ("hevc", vec!["hevc", "h264"]),
                                ("h264", vec!["h264"]),
                                ("av1", vec!["av1", "hevc", "h264"]),
                                ("pyrowave", vec!["pyrowave", "hevc", "h264"]),
                            ] {
                                if ui.selectable_label(current == value, codec_label(value)).clicked() {
                                    c.codecs = list.into_iter().map(String::from).collect();
                                }
                            }
                        });
                    ui.end_row();
                    ui.label("Bitrate (HEVC/H.264)");
                    let mut mbps = c.bitrate_kbps / 1000;
                    ui.add(egui::Slider::new(&mut mbps, 2..=200).suffix(" Mbit/s").logarithmic(true));
                    c.bitrate_kbps = mbps * 1000;
                    ui.end_row();
                    ui.label("Bitrate (PyroWave)");
                    let mut mbps = c.pyrowave_kbps / 1000;
                    ui.add(egui::Slider::new(&mut mbps, 50..=1500).suffix(" Mbit/s").logarithmic(true));
                    c.pyrowave_kbps = mbps * 1000;
                    ui.end_row();
                    ui.label("Frame rate");
                    egui::ComboBox::from_id_salt("fps")
                        .selected_text(format!("{} fps", c.max_fps))
                        .show_ui(ui, |ui| {
                            for fps in [30, 60, 90, 120, 144, 165, 240] {
                                ui.selectable_value(&mut c.max_fps, fps, format!("{fps} fps"));
                            }
                        });
                    ui.end_row();
                    ui.label("Mouse");
                    egui::ComboBox::from_id_salt("mouse")
                        .selected_text(match c.mouse.as_str() {
                            "desktop" => "Desktop",
                            "captured" => "Captured (games)",
                            _ => "Auto",
                        })
                        .show_ui(ui, |ui| {
                            for (v, l) in [("auto", "Auto"), ("desktop", "Desktop"), ("captured", "Captured (games)")] {
                                if ui.selectable_label(c.mouse == v, l).clicked() {
                                    c.mouse = v.into();
                                }
                            }
                        });
                    ui.end_row();
                    ui.label("Menu shortcut");
                    ui.text_edit_singleline(&mut self.shortcut_text);
                    ui.end_row();
                });
                ui.checkbox(&mut c.audio, "Sound");
                ui.checkbox(&mut c.fullscreen, "Start in fullscreen");
                ui.checkbox(&mut c.show_stats, "Show statistics");
                if ui.button("Save settings").clicked() {
                    actions.push(Action::SaveSettings);
                }
            });
        });
        actions
    }
}

fn codec_label(value: &str) -> &'static str {
    match value {
        "auto" => "Automatic (PyroWave on fast links, else HEVC)",
        "h264" => "H.264",
        "av1" => "AV1",
        "pyrowave" => "PyroWave",
        _ => "HEVC",
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
        if egui.on_window_event(window, &event).repaint {
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
                let output_repaint_now = output
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
                let repaint = !actions.is_empty() || output_repaint_now;
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
        // Debugging aid: ORIEL_DEBUG_SCREENSHOT=path saves the launcher and quits.
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
            || self.reconnect.is_some();
        event_loop.set_control_flow(if waiting {
            ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(100))
        } else {
            ControlFlow::Wait
        });
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        self.gfx = None;
    }
}
