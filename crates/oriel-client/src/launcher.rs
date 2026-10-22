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
    app::{self, ViewerExit, ViewerPrefs},
    config::{
        CODEC_CHOICES, ClientConfig, HostPrefs, codec_choice, format_resolution, parse_resolution,
    },
    connection::{self, Connected},
    keymap::{Bindings, Command, Shortcut},
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

// Palette, matching the icon.
const BG: egui::Color32 = egui::Color32::from_rgb(0x17, 0x1a, 0x26);
const CARD: egui::Color32 = egui::Color32::from_rgb(0x22, 0x27, 0x38);
const CARD_HOVER: egui::Color32 = egui::Color32::from_rgb(0x2c, 0x33, 0x4a);
const FRAME: egui::Color32 = egui::Color32::from_rgb(0x2a, 0x2f, 0x45);
const AMBER: egui::Color32 = egui::Color32::from_rgb(0xff, 0xc4, 0x4d);
const BLUE: egui::Color32 = egui::Color32::from_rgb(0x6c, 0x9c, 0xf0);
const DEEP_BLUE: egui::Color32 = egui::Color32::from_rgb(0x3d, 0x5a, 0x99);
const MUTED: egui::Color32 = egui::Color32::from_rgb(0x9a, 0xa3, 0xb8);
const GREEN: egui::Color32 = egui::Color32::from_rgb(0x5f, 0xd0, 0x7f);

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

/// The app's look: dark, rounded, a little roomier than egui's default.
pub(crate) fn style(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = BG;
    visuals.window_fill = CARD;
    visuals.extreme_bg_color = egui::Color32::from_rgb(0x12, 0x14, 0x1d);
    visuals.selection.bg_fill = DEEP_BLUE;
    visuals.hyperlink_color = BLUE;
    visuals.window_corner_radius = egui::CornerRadius::same(12);
    for w in [
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.noninteractive,
        &mut visuals.widgets.open,
    ] {
        w.corner_radius = egui::CornerRadius::same(8);
    }
    visuals.widgets.inactive.weak_bg_fill = CARD_HOVER;
    visuals.widgets.inactive.bg_fill = CARD_HOVER;
    visuals.widgets.hovered.weak_bg_fill = egui::Color32::from_rgb(0x38, 0x41, 0x5e);
    visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(0x38, 0x41, 0x5e);
    visuals.widgets.active.weak_bg_fill = DEEP_BLUE;
    ctx.set_visuals(visuals);
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = egui::vec2(10.0, 8.0);
        s.spacing.button_padding = egui::vec2(12.0, 6.0);
        s.spacing.interact_size.y = 28.0;
        for (text_style, size) in [
            (egui::TextStyle::Body, 15.0),
            (egui::TextStyle::Button, 15.0),
            (egui::TextStyle::Heading, 24.0),
            (egui::TextStyle::Small, 12.0),
        ] {
            if let Some(f) = s.text_styles.get_mut(&text_style) {
                f.size = size;
            }
        }
    });
}

fn primary_button(text: &str) -> egui::Button<'static> {
    egui::Button::new(egui::RichText::new(text.to_string()).color(egui::Color32::BLACK)).fill(AMBER)
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
    let config = ClientConfig::load(&paths).unwrap_or_default();
    let mut launcher = Launcher {
        has_tray: tray.is_some(),
        #[cfg(windows)]
        tray,
        window_wanted: !(options.start_hidden && options.connect.is_none()),
        log_path: options.log_path.clone(),
        quit_flag: options.quit_flag.clone(),
        quit: false,
        tray_updated: Instant::now(),
        bindings: config.bindings(),
        recording: None,
        shortcut_hint: String::new(),
        playback_devices: Vec::new(),
        microphones: Vec::new(),
        config,
        paths,
        host,
        rt: Arc::new(rt),
        gfx: None,
        egui_ctx: egui::Context::default(),
        hosts: Vec::new(),
        connecting: None,
        pairing: None,
        pair_dialog: None,
        settings_open: false,
        settings_tab: SettingsTab::Video,
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
        Ok("settings") => launcher.settings_open = true,
        Ok("settings-input") => {
            launcher.settings_open = true;
            launcher.settings_tab = SettingsTab::Input;
        }
        Ok("settings-sound") => {
            launcher.settings_open = true;
            launcher.settings_tab = SettingsTab::Sound;
        }
        Ok("settings-window") => {
            launcher.settings_open = true;
            launcher.settings_tab = SettingsTab::Window;
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
        let explicit = options.viewer.is_some();
        let viewer_options = match &options.viewer {
            Some(v) => ViewerOptions {
                target: target.clone(),
                ..v.clone()
            },
            None => launcher.viewer_options(&target)?,
        };
        let (exit, prefs) = app::run(&mut event_loop, &launcher.rt, connected, viewer_options)?;
        tracing::info!(?exit, "viewer ended");
        if !explicit {
            launcher.remember(&target, prefs);
        }
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

#[derive(Default)]
struct PairDialog {
    address: String,
    pin: String,
    name: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsTab {
    Video,
    Input,
    Window,
    Sound,
}

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
    pair_dialog: Option<PairDialog>,
    settings_open: bool,
    settings_tab: SettingsTab,
    status: String,
    reconnect: Option<Reconnect>,
    result: Option<LauncherResult>,
    /// Shortcuts being edited in the settings dialog.
    bindings: Bindings,
    /// The command whose shortcut the next key press sets.
    recording: Option<Command>,
    shortcut_hint: String,
    /// Local sound outputs, listed when the settings dialog opens.
    playback_devices: Vec<oriel_audio::OutputDevice>,
    microphones: Vec<oriel_audio::OutputDevice>,
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
    CloseSettings,
    SaveSettings,
    StartHosting,
    StopHosting,
    AllowPairing,
    Kick(usize),
    CancelReconnect,
    SetHostFullscreen(String, bool),
    SetHostCodec(String, Option<String>),
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

    fn prefs(&self, host: &str) -> HostPrefs {
        self.config.hosts.get(host).cloned().unwrap_or_default()
    }

    fn save_config(&mut self) {
        if let Err(e) = self.config.save(&self.paths) {
            self.status = format!("Cannot save settings: {e:#}");
        }
    }

    /// Remember the choices made during a session with `host`.
    fn remember(&mut self, host: &str, prefs: ViewerPrefs) {
        // Where the menu button was dragged applies to every host.
        if prefs.menu_button_pos != self.config.menu_button_pos {
            self.config.menu_button_pos = prefs.menu_button_pos;
            self.save_config();
        }
        let new = HostPrefs {
            fullscreen: Some(prefs.fullscreen),
            codec: Some(prefs.codec_key),
            resolution: Some(format_resolution(prefs.resolution)),
            audio_device: prefs.audio_device,
            virtual_display: prefs
                .virtual_display
                .as_ref()
                .map(crate::config::format_virtual_mode),
            microphone: Some(prefs.microphone),
        };
        let entry = self.config.hosts.entry(host.to_string()).or_default();
        if *entry != new {
            *entry = new;
            self.save_config();
        }
    }

    fn viewer_options(&self, target: &str) -> Result<ViewerOptions> {
        let c = &self.config;
        let prefs = self.prefs(target);
        let codec_key = prefs.codec.clone().unwrap_or_else(|| c.codec_key());
        let (codecs, auto_codec) = codec_choice(&codec_key);
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
                resolution: prefs.resolution.as_deref().and_then(parse_resolution),
                audio_device: prefs.audio_device.clone(),
                virtual_display: prefs
                    .virtual_display
                    .as_deref()
                    .and_then(crate::config::parse_virtual_mode),
            },
            hardware_decode: true,
            shortcuts: c.bindings(),
            mouse_mode: c.mouse_mode()?,
            fullscreen: prefs.fullscreen.unwrap_or(c.fullscreen),
            show_stats: c.show_stats,
            screenshot: None,
            codec_key,
            playback_device: c.playback_device.clone(),
            gamepads: c.gamepads,
            microphone: prefs.microphone.unwrap_or(c.microphone),
            microphone_device: c.microphone_device.clone(),
            menu_button: c.menu_button.clone(),
            menu_button_pos: c.menu_button_pos,
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
            Action::OpenSettings => {
                self.bindings = self.config.bindings();
                self.recording = None;
                self.shortcut_hint.clear();
                self.playback_devices = oriel_audio::output_devices();
                self.microphones = oriel_audio::input_devices();
                self.settings_open = true;
            }
            Action::CloseSettings => {
                // Discard unsaved edits.
                self.config = ClientConfig::load(&self.paths).unwrap_or_default();
                self.recording = None;
                self.settings_open = false;
            }
            Action::SaveSettings => {
                self.config.set_bindings(&self.bindings);
                self.recording = None;
                self.save_config();
                self.settings_open = false;
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
            Action::SetHostFullscreen(host, value) => {
                self.config.hosts.entry(host).or_default().fullscreen = Some(value);
                self.save_config();
            }
            Action::SetHostCodec(host, value) => {
                self.config.hosts.entry(host).or_default().codec = value;
                self.save_config();
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
                        self.pair_dialog = None;
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
                let (rect, response) = ui.allocate_exact_size(tile, egui::Sense::click());
                let hovered = response.hovered() && !busy;
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
                painter.rect_filled(screen, 6, FRAME);
                painter.rect_filled(
                    screen.shrink(5.0),
                    3,
                    if hovered { BLUE } else { DEEP_BLUE },
                );
                let stand = egui::Rect::from_center_size(
                    screen.center_bottom() + egui::vec2(0.0, 8.0),
                    egui::vec2(30.0, 4.0),
                );
                painter.rect_filled(stand, 2, FRAME);
                painter.text(
                    rect.center_top() + egui::vec2(0.0, 108.0),
                    egui::Align2::CENTER_CENTER,
                    &h.name,
                    egui::FontId::proportional(18.0),
                    egui::Color32::WHITE,
                );
                painter.text(
                    rect.center_top() + egui::vec2(0.0, 132.0),
                    egui::Align2::CENTER_CENTER,
                    h.address.as_deref().unwrap_or(""),
                    egui::FontId::proportional(12.5),
                    MUTED,
                );
                if connecting {
                    let spin =
                        egui::Rect::from_center_size(screen.center(), egui::vec2(22.0, 22.0));
                    ui.put(spin, egui::Spinner::new().color(egui::Color32::WHITE));
                }
                let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
                if response.clicked() && !busy {
                    actions.push(Action::Connect(h.name.clone()));
                }
                let prefs = self.prefs(&h.name);
                response.context_menu(|ui| {
                    if ui.button("Connect").clicked() {
                        actions.push(Action::Connect(h.name.clone()));
                        ui.close();
                    }
                    ui.separator();
                    let mut full = prefs.fullscreen.unwrap_or(self.config.fullscreen);
                    if ui.checkbox(&mut full, "Open in fullscreen").changed() {
                        actions.push(Action::SetHostFullscreen(h.name.clone(), full));
                    }
                    ui.menu_button("Codec", |ui| {
                        let current = prefs.codec.clone();
                        if ui
                            .radio(current.is_none(), "Default (from Settings)")
                            .clicked()
                        {
                            actions.push(Action::SetHostCodec(h.name.clone(), None));
                            ui.close();
                        }
                        for (key, label) in CODEC_CHOICES {
                            if ui.radio(current.as_deref() == Some(key), label).clicked() {
                                actions
                                    .push(Action::SetHostCodec(h.name.clone(), Some(key.into())));
                                ui.close();
                            }
                        }
                    });
                    if let Some((w, hgt)) = prefs.resolution.as_deref().and_then(parse_resolution) {
                        ui.label(
                            egui::RichText::new(format!(
                                "Resolution {w}×{hgt} (change it while connected)"
                            ))
                            .color(MUTED),
                        );
                    }
                    ui.separator();
                    if ui.button(format!("Forget {}", h.name)).clicked() {
                        actions.push(Action::Forget(h.name.clone()));
                        ui.close();
                    }
                });
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

    /// Take the next key press as the shortcut for `command`.
    fn record_shortcut(&mut self, ui: &mut egui::Ui, command: Command) {
        let events = ui.input(|i| i.events.clone());
        for event in events {
            let egui::Event::Key {
                key,
                physical_key,
                pressed: true,
                modifiers: m,
                ..
            } = event
            else {
                continue;
            };
            if key == egui::Key::Escape && m.is_none() {
                // Cancel recording without closing the dialog.
                ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
                self.recording = None;
                return;
            }
            let logo = cfg!(target_os = "macos") && m.mac_cmd;
            if !(m.ctrl || m.alt || m.shift || logo) {
                self.shortcut_hint =
                    "Include Ctrl, Alt or Shift, so the key still reaches the host.".into();
                continue;
            }
            // Shortcuts name physical keys, like everything sent to the host.
            let key = physical_key.unwrap_or(key);
            let text = format!(
                "{}{}{}{}{}",
                if m.ctrl { "Ctrl+" } else { "" },
                if m.alt { "Alt+" } else { "" },
                if m.shift { "Shift+" } else { "" },
                if logo { "Cmd+" } else { "" },
                key.name()
            );
            let Some(shortcut) = Shortcut::parse(&text) else {
                self.shortcut_hint = format!("{} cannot be used in a shortcut.", key.name());
                continue;
            };
            // One shortcut, one command.
            for other in Command::ALL {
                if other != command && self.bindings.get(other) == Some(shortcut) {
                    if other == Command::Menu {
                        self.shortcut_hint = format!("{shortcut} already opens the menu.");
                        return;
                    }
                    self.bindings.set(other, None);
                    self.shortcut_hint = format!("Removed {shortcut} from {}.", other.label());
                }
            }
            self.bindings.set(command, Some(shortcut));
            self.recording = None;
            return;
        }
    }

    fn settings_modal(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        if !self.settings_open {
            return;
        }
        let modal = egui::Modal::new(egui::Id::unique("settings"))
            .frame(dialog_frame())
            .show(ctx, |ui| {
                let screen = ui.ctx().content_rect();
                // Dialog padding and the title, tabs and buttons around
                // the scrolling part.
                let width = (screen.width() - 92.0).clamp(220.0, 500.0);
                let height = (screen.height() - 230.0).clamp(120.0, 350.0);
                let field = (width - 170.0).clamp(150.0, 280.0);
                ui.set_width(width);
                if let Some(command) = self.recording {
                    self.record_shortcut(ui, command);
                }
                ui.heading("Settings");
                ui.horizontal(|ui| {
                    for (tab, label) in [
                        (SettingsTab::Video, "Video"),
                        (SettingsTab::Input, "Input"),
                        (SettingsTab::Window, "Window"),
                        (SettingsTab::Sound, "Sound"),
                    ] {
                        ui.selectable_value(&mut self.settings_tab, tab, label);
                    }
                });
                ui.separator();
                // The same size on every tab, within the window; content
                // that does not fit scrolls.
                egui::ScrollArea::both()
                    .id_salt("settings-scroll")
                    .max_width(width)
                    .max_height(height)
                    .auto_shrink(false)
                    .show(ui, |ui| {
                        let c = &mut self.config;
                        egui::Grid::new("settings-grid")
                            .num_columns(2)
                            .spacing([16.0, 12.0])
                            .show(ui, |ui| match self.settings_tab {
                                SettingsTab::Video => {
                                    ui.label("Codec");
                                    let current = c.codec_key();
                                    egui::ComboBox::from_id_salt("codec")
                                        .width(field)
                                        .selected_text(codec_label(&current))
                                        .show_ui(ui, |ui| {
                                            for (key, _) in CODEC_CHOICES {
                                                if ui
                                                    .selectable_label(
                                                        current == key,
                                                        codec_label(key),
                                                    )
                                                    .clicked()
                                                {
                                                    c.codecs = codec_choice_strings(key);
                                                }
                                            }
                                        });
                                    ui.end_row();
                                    ui.label("Bitrate (HEVC, H.264, AV1)");
                                    let mut mbps = c.bitrate_kbps / 1000;
                                    ui.add(
                                        egui::Slider::new(&mut mbps, 2..=200)
                                            .suffix(" Mbit/s")
                                            .logarithmic(true),
                                    );
                                    c.bitrate_kbps = mbps * 1000;
                                    ui.end_row();
                                    ui.label("Bitrate (PyroWave)");
                                    let mut mbps = c.pyrowave_kbps / 1000;
                                    ui.add(
                                        egui::Slider::new(&mut mbps, 50..=1500)
                                            .suffix(" Mbit/s")
                                            .logarithmic(true),
                                    );
                                    c.pyrowave_kbps = mbps * 1000;
                                    ui.end_row();
                                    ui.label("Frame rate");
                                    egui::ComboBox::from_id_salt("fps")
                                        .width(field)
                                        .selected_text(format!("{} fps", c.max_fps))
                                        .show_ui(ui, |ui| {
                                            for fps in [30, 60, 90, 120, 144, 165, 240] {
                                                ui.selectable_value(
                                                    &mut c.max_fps,
                                                    fps,
                                                    format!("{fps} fps"),
                                                );
                                            }
                                        });
                                    ui.end_row();
                                }
                                SettingsTab::Input => {
                                    ui.label("Mouse");
                                    egui::ComboBox::from_id_salt("mouse")
                                        .width(field)
                                        .selected_text(match c.mouse.as_str() {
                                            "desktop" => "Desktop",
                                            "captured" => "Captured (games)",
                                            _ => "Automatic",
                                        })
                                        .show_ui(ui, |ui| {
                                            for (v, l) in [
                                                ("auto", "Automatic"),
                                                ("desktop", "Desktop"),
                                                ("captured", "Captured (games)"),
                                            ] {
                                                if ui.selectable_label(c.mouse == v, l).clicked() {
                                                    c.mouse = v.into();
                                                }
                                            }
                                        });
                                    ui.end_row();
                                    ui.label("Gamepads");
                                    ui.checkbox(&mut c.gamepads, "Forward controllers");
                                    ui.end_row();
                                    for command in Command::ALL {
                                        ui.label(command.label());
                                        ui.horizontal(|ui| {
                                            let recording = self.recording == Some(command);
                                            let current = self.bindings.get(command);
                                            let text = if recording {
                                                "Press the new shortcut...".to_string()
                                            } else {
                                                current.map_or_else(
                                                    || "None".into(),
                                                    |s| s.to_string(),
                                                )
                                            };
                                            let button = egui::Button::new(text)
                                                .min_size(egui::vec2(field.min(210.0), 0.0))
                                                .selected(recording);
                                            let response = ui.add(button).on_hover_text(
                                                "Click, then press the keys (Esc cancels)",
                                            );
                                            if response.clicked() {
                                                // Keep Space/Enter from clicking it again.
                                                response.surrender_focus();
                                                self.recording = (!recording).then_some(command);
                                                self.shortcut_hint.clear();
                                            }
                                            if command != Command::Menu
                                                && current.is_some()
                                                && ui.small_button("Clear").clicked()
                                            {
                                                self.bindings.set(command, None);
                                            }
                                            let default =
                                                Shortcut::parse(command.default_shortcut());
                                            if current != default
                                                && ui.small_button("Reset").clicked()
                                            {
                                                self.bindings.set(command, default);
                                            }
                                        });
                                        ui.end_row();
                                    }
                                    if !self.shortcut_hint.is_empty() {
                                        ui.label("");
                                        ui.colored_label(AMBER, &self.shortcut_hint);
                                        ui.end_row();
                                    }
                                }
                                SettingsTab::Window => {
                                    ui.label("Fullscreen");
                                    ui.checkbox(&mut c.fullscreen, "Start in fullscreen");
                                    ui.end_row();
                                    ui.label("Statistics");
                                    ui.checkbox(&mut c.show_stats, "Show statistics");
                                    ui.end_row();
                                    ui.label("Menu button");
                                    let label = |key: &str| {
                                        crate::config::MENU_BUTTON_STYLES
                                            .iter()
                                            .find(|(k, _)| *k == key)
                                            .map_or("Logo and shortcut", |(_, l)| *l)
                                    };
                                    egui::ComboBox::from_id_salt("menu-button")
                                        .width(field)
                                        .selected_text(label(&c.menu_button))
                                        .show_ui(ui, |ui| {
                                            for (key, l) in crate::config::MENU_BUTTON_STYLES {
                                                if ui
                                                    .selectable_label(c.menu_button == key, l)
                                                    .clicked()
                                                {
                                                    c.menu_button = key.into();
                                                }
                                            }
                                        });
                                    ui.end_row();
                                    ui.label("");
                                    ui.add_enabled_ui(c.menu_button_pos.is_some(), |ui| {
                                        if ui
                                            .button("Reset position")
                                            .on_hover_text(
                                                "Drag the button to move it while connected",
                                            )
                                            .clicked()
                                        {
                                            c.menu_button_pos = None;
                                        }
                                    });
                                    ui.end_row();
                                }
                                SettingsTab::Sound => {
                                    ui.label("Sound");
                                    ui.checkbox(&mut c.audio, "Play remote sound");
                                    ui.end_row();
                                    ui.label("Play on");
                                    let devices = &self.playback_devices;
                                    let name = |id: &Option<String>| match id {
                                        None => "Default output".to_string(),
                                        Some(id) => {
                                            devices.iter().find(|d| &d.id == id).map_or_else(
                                                || format!("{id} (not found)"),
                                                |d| d.name.clone(),
                                            )
                                        }
                                    };
                                    egui::ComboBox::from_id_salt("playback-device")
                                        .selected_text(name(&c.playback_device))
                                        .width(field)
                                        .show_ui(ui, |ui| {
                                            let choices = std::iter::once(None)
                                                .chain(devices.iter().map(|d| Some(d.id.clone())));
                                            for choice in choices {
                                                let label = name(&choice);
                                                ui.selectable_value(
                                                    &mut c.playback_device,
                                                    choice,
                                                    label,
                                                );
                                            }
                                        });
                                    ui.end_row();
                                    ui.label("Microphone");
                                    ui.checkbox(&mut c.microphone, "Send my microphone");
                                    ui.end_row();
                                    ui.label("");
                                    let mics = &self.microphones;
                                    let mic_name = |id: &Option<String>| match id {
                                        None => "Default microphone".to_string(),
                                        Some(id) => mics.iter().find(|d| &d.id == id).map_or_else(
                                            || format!("{id} (not found)"),
                                            |d| d.name.clone(),
                                        ),
                                    };
                                    egui::ComboBox::from_id_salt("microphone-device")
                                        .selected_text(mic_name(&c.microphone_device))
                                        .width(field)
                                        .show_ui(ui, |ui| {
                                            let choices = std::iter::once(None)
                                                .chain(mics.iter().map(|d| Some(d.id.clone())));
                                            for choice in choices {
                                                let label = mic_name(&choice);
                                                ui.selectable_value(
                                                    &mut c.microphone_device,
                                                    choice,
                                                    label,
                                                );
                                            }
                                        });
                                    ui.end_row();
                                }
                            });
                        if self.settings_tab == SettingsTab::Sound {
                            ui.add_space(12.0);
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(
                                        "Which of the remote computer's outputs you hear, and the \
                                 microphone, can also be changed in the menu while connected.",
                                    )
                                    .color(MUTED)
                                    .size(12.0),
                                )
                                .wrap(),
                            );
                        }
                    });
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.add(primary_button("Save")).clicked() {
                        actions.push(Action::SaveSettings);
                    }
                    if ui.button("Cancel").clicked() {
                        actions.push(Action::CloseSettings);
                    }
                });
            });
        if modal.should_close() {
            actions.push(Action::CloseSettings);
        }
    }
}

fn dialog_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(CARD)
        .corner_radius(14)
        .inner_margin(22)
        .stroke(egui::Stroke::new(1.0, CARD_HOVER))
}

fn section_title(ui: &mut egui::Ui, text: &str) {
    ui.label(
        egui::RichText::new(text.to_uppercase())
            .size(12.0)
            .strong()
            .color(MUTED),
    );
    ui.add_space(4.0);
}

pub(crate) fn paint_logo(painter: &egui::Painter, rect: egui::Rect) {
    painter.rect_filled(rect, 6, FRAME);
    let inner = rect.shrink(3.0);
    let half = inner.width() / 2.0 - 1.0;
    let panes = [
        (inner.left_top(), AMBER),
        (inner.left_top() + egui::vec2(half + 2.0, 0.0), BLUE),
        (inner.left_top() + egui::vec2(0.0, half + 2.0), BLUE),
        (inner.left_top() + egui::vec2(half + 2.0, half + 2.0), BLUE),
    ];
    for (pos, color) in panes {
        painter.rect_filled(
            egui::Rect::from_min_size(pos, egui::vec2(half, half)),
            2,
            color,
        );
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

/// The `codecs` list stored in client.toml for a choice key.
fn codec_choice_strings(key: &str) -> Vec<String> {
    match key {
        "auto" => vec!["auto".into()],
        "h264" => vec!["h264".into()],
        "av1" => vec!["av1".into(), "hevc".into(), "h264".into()],
        "pyrowave" => vec!["pyrowave".into(), "hevc".into(), "h264".into()],
        _ => vec!["hevc".into(), "h264".into()],
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
            || self.reconnect.is_some()
            || std::env::var_os("ORIEL_DEBUG_SCREENSHOT").is_some();
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
