//! The viewer window: draws the remote surface, forwards input, manages the
//! client-side cursor and the overlay menu.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Result;
use oriel_proto::{
    ClientMsg, CursorShape, CursorState, HostMsg, InputEvent, MouseButton, StreamSettings,
    SurfaceInfo, VirtualMode,
};
use parking_lot::Mutex;
use winit::{
    application::ApplicationHandler,
    dpi::{PhysicalPosition, PhysicalSize},
    event::{
        DeviceEvent, DeviceId, ElementState, KeyEvent, MouseScrollDelta, StartCause, WindowEvent,
    },
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    keyboard::{ModifiersState, PhysicalKey},
    window::{CursorGrabMode, CursorIcon, CustomCursor, Fullscreen, Window, WindowId},
};

use crate::{
    config::{ClientConfig, Effective},
    connection,
    keymap::{Bindings, Command, hid_usage},
    render::{Renderer, Viewport, letterbox},
    session::{self, Session, UiEvent},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseMode {
    /// Relative while an application on the host has captured the cursor.
    Auto,
    Absolute,
    Relative,
}

#[derive(Debug, Clone)]
pub struct ViewerOptions {
    /// The host's name in the config (its settings live there).
    pub target: String,
    /// The settings; changes made in the menu are saved to `paths`.
    pub config: ClientConfig,
    pub paths: Option<oriel_net::Paths>,
    /// What to ask the host for at the start (from `config`).
    pub settings: StreamSettings,
    pub hardware_decode: bool,
    /// Debugging: save a screenshot after this long, then exit.
    pub screenshot: Option<(PathBuf, Duration)>,
}

impl ViewerOptions {
    /// Options for `target` from the saved settings.
    pub fn from_config(
        target: &str,
        config: ClientConfig,
        paths: Option<oriel_net::Paths>,
    ) -> Self {
        let e = config.effective(Some(target));
        let (codecs, auto_codec) = crate::config::codec_choice(&e.codec);
        Self {
            target: target.to_string(),
            settings: StreamSettings {
                display: None,
                codecs,
                bitrate_kbps: e.bitrate_kbps,
                max_fps: e.max_fps,
                audio: e.audio,
                auto_codec,
                pyrowave_kbps: e.pyrowave_kbps,
                resolution: e.resolution,
                audio_device: e.audio_device.clone(),
                virtual_display: e.virtual_display,
            },
            config,
            paths,
            hardware_decode: true,
            screenshot: None,
        }
    }
}

/// Why the viewer window went away.
#[derive(Debug)]
pub enum ViewerExit {
    /// The window was closed.
    Closed,
    /// The user disconnected from the menu.
    Back,
    /// The connection ended; `retry` when it was lost rather than closed by
    /// the host.
    Lost { reason: String, retry: bool },
}

/// Show a connected session until it ends.
pub fn run(
    event_loop: &mut EventLoop<UiEvent>,
    rt: &tokio::runtime::Runtime,
    connected: connection::Connected,
    options: ViewerOptions,
) -> Result<ViewerExit> {
    use winit::platform::run_on_demand::EventLoopExtRunOnDemand;
    tracing::info!(host = %connected.host_name, "connected");
    let proxy = Mutex::new(event_loop.create_proxy());
    let sink: session::Sink = Arc::new(move |ev| {
        let _ = proxy.lock().send_event(ev);
    });
    let e = options.config.effective(Some(&options.target));
    let session = session::start(
        rt.handle(),
        connected.conn.clone(),
        connected.ctl,
        session::SessionOptions {
            settings: options.settings.clone(),
            hardware_decode: options.hardware_decode,
            playback_device: e.playback_device.clone(),
            gamepads: e.gamepads,
            microphone: e.microphone,
            microphone_device: e.microphone_device.clone(),
        },
        sink.clone(),
    );
    let mut viewer = Viewer::new(session.clone(), connected.host_name, options);
    viewer.sink = Some(sink.clone());
    viewer.displays = connected.displays;
    viewer.host_codecs = connected.codecs;
    viewer.audio_devices = connected.audio_devices;
    viewer.virtual_ok = connected.virtual_display;
    event_loop.run_app_on_demand(&mut viewer)?;
    session.close();
    rt.block_on(async {
        tokio::time::sleep(Duration::from_millis(150)).await;
        let _ = tokio::time::timeout(Duration::from_secs(1), connected.endpoint.wait_idle()).await;
    });
    viewer.save();
    Ok(viewer.exit.take().unwrap_or(ViewerExit::Closed))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppliedCursor {
    Hidden,
    Default,
    Remote(u64),
}

struct Gfx {
    window: Arc<Window>,
    renderer: Renderer,
    egui: egui_winit::State,
    clipboard: egui_winit::clipboard::Clipboard,
    #[cfg(target_os = "linux")]
    inhibitor: Option<crate::inhibit::ShortcutInhibitor>,
}

struct Viewer {
    session: Session,
    host_name: String,
    options: ViewerOptions,
    gfx: Option<Gfx>,
    egui_ctx: egui::Context,

    surface: Option<SurfaceInfo>,
    sized_window: bool,
    shapes: HashMap<u64, CursorShape>,
    os_cursors: HashMap<u64, CustomCursor>,
    egui_cursors: HashMap<u64, egui::TextureHandle>,
    host_cursor: Option<CursorState>,
    applied: Option<AppliedCursor>,
    grabbed: bool,

    menu_open: bool,
    /// The settings, and those currently in force (see `sync`).
    config: ClientConfig,
    in_force: Effective,
    config_dirty: bool,
    shortcuts: Bindings,
    panel: crate::settings_ui::Panel,
    focused: bool,
    modifiers: ModifiersState,
    keys_down: HashSet<u16>,
    buttons_down: HashSet<MouseButton>,
    pointer: Option<PhysicalPosition<f64>>,
    last_move: Option<(i32, i32)>,
    pointer_over_ui: bool,
    wheel_px: (f64, f64),
    motion: (f64, f64),

    notices: Vec<(String, Instant)>,
    closed: Option<String>,
    first_frame: Option<Instant>,
    frames_presented: u64,
    latency_ms: f32,
    screenshot_taken: bool,
    exit: Option<ViewerExit>,
    clipboard_last: Option<String>,
    clipboard_polled: Instant,
    displays: Vec<oriel_proto::DisplayInfo>,
    display: Option<u32>,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    shortcuts_inhibited: bool,
    host_codecs: Vec<oriel_proto::Codec>,
    audio_devices: Vec<oriel_proto::AudioDevice>,
    /// The host can create a virtual monitor for this window.
    virtual_ok: bool,
    /// Where the menu was dragged (fractions of the window), and whether it
    /// has been placed since it opened.
    menu_pos: Option<[f32; 2]>,
    menu_placed: bool,
    /// The window size the menu was placed for.
    menu_window: Option<PhysicalSize<u32>>,
    /// Bumped whenever the menu is placed (see `overlay`).
    menu_generation: u32,
    sink: Option<session::Sink>,
    #[cfg(windows)]
    key_hook: Option<crate::winkeys::KeyboardHook>,
}

impl Viewer {
    fn new(session: Session, host_name: String, options: ViewerOptions) -> Self {
        let egui_ctx = egui::Context::default();
        crate::theme::style(&egui_ctx);
        Self {
            session,
            host_name,
            gfx: None,
            egui_ctx,
            surface: None,
            sized_window: false,
            shapes: HashMap::new(),
            os_cursors: HashMap::new(),
            egui_cursors: HashMap::new(),
            host_cursor: None,
            applied: None,
            grabbed: false,
            // Debugging aid for screenshots.
            menu_open: std::env::var_os("ORIEL_DEBUG_MENU").is_some(),
            in_force: options.config.effective(Some(&options.target)),
            shortcuts: options.config.bindings(),
            config: options.config.clone(),
            config_dirty: false,
            panel: crate::settings_ui::Panel::new(true),
            focused: false,
            modifiers: ModifiersState::empty(),
            keys_down: HashSet::new(),
            buttons_down: HashSet::new(),
            pointer: None,
            last_move: None,
            pointer_over_ui: false,
            wheel_px: (0.0, 0.0),
            motion: (0.0, 0.0),
            notices: Vec::new(),
            closed: None,
            first_frame: None,
            frames_presented: 0,
            latency_ms: 0.0,
            screenshot_taken: false,
            exit: None,
            clipboard_last: None,
            clipboard_polled: Instant::now(),
            displays: Vec::new(),
            display: options.settings.display,
            shortcuts_inhibited: false,
            host_codecs: Vec::new(),
            audio_devices: Vec::new(),
            virtual_ok: false,
            menu_pos: None,
            menu_placed: false,
            menu_window: None,
            menu_generation: 0,
            sink: None,
            #[cfg(windows)]
            key_hook: None,
            options,
        }
    }

    fn window(&self) -> Option<&Arc<Window>> {
        self.gfx.as_ref().map(|g| &g.window)
    }

    fn request_redraw(&self) {
        if let Some(w) = self.window() {
            w.request_redraw();
        }
    }

    /// Is input currently going to the host?
    fn forwarding(&self) -> bool {
        !self.menu_open && self.closed.is_none() && self.focused
    }

    fn relative(&self) -> bool {
        let wanted = match self.in_force.mouse_mode() {
            MouseMode::Auto => self.host_cursor.is_some_and(|c| c.captured),
            MouseMode::Absolute => false,
            MouseMode::Relative => true,
        };
        wanted && self.forwarding()
    }

    fn viewport(&self) -> Option<Viewport> {
        let gfx = self.gfx.as_ref()?;
        let video = self
            .surface
            .as_ref()
            .map(|s| (s.width, s.height))
            .or(gfx.renderer.video_size())?;
        Some(letterbox(
            (gfx.renderer.config.width, gfx.renderer.config.height),
            video,
        ))
    }

    fn to_surface(&self, p: PhysicalPosition<f64>) -> Option<(i32, i32)> {
        let surface = self.surface.as_ref()?;
        let vp = self.viewport()?;
        let x = (p.x as f32 - vp.x) * surface.width as f32 / vp.w;
        let y = (p.y as f32 - vp.y) * surface.height as f32 / vp.h;
        Some((
            (x.floor() as i32).clamp(0, surface.width as i32 - 1),
            (y.floor() as i32).clamp(0, surface.height as i32 - 1),
        ))
    }

    fn release_all(&mut self) {
        if !self.keys_down.is_empty() || !self.buttons_down.is_empty() {
            self.session.input(InputEvent::ReleaseAll);
        }
        self.keys_down.clear();
        self.buttons_down.clear();
    }

    fn set_menu(&mut self, open: bool) {
        if self.menu_open == open {
            return;
        }
        self.menu_open = open;
        self.menu_placed = false;
        self.release_all();
        self.update_cursor();
        self.request_redraw();
    }

    fn update_cursor(&mut self) {
        #[cfg(windows)]
        if let Some(hook) = &self.key_hook {
            hook.set_active(self.forwarding());
        }
        #[cfg(target_os = "linux")]
        {
            let forwarding = self.forwarding();
            if let Some(i) = self.gfx.as_mut().and_then(|g| g.inhibitor.as_mut()) {
                i.set(forwarding);
            }
        }
        let relative = self.relative();
        let Some(window) = self.window().cloned() else {
            return;
        };
        if relative != self.grabbed {
            if relative {
                let locked = window
                    .set_cursor_grab(CursorGrabMode::Locked)
                    .or_else(|_| window.set_cursor_grab(CursorGrabMode::Confined));
                if let Err(e) = locked {
                    tracing::warn!("cannot capture the pointer: {e}");
                }
                self.motion = (0.0, 0.0);
            } else {
                let _ = window.set_cursor_grab(CursorGrabMode::None);
            }
            self.grabbed = relative;
        }
        let host_visible = self.host_cursor.is_none_or(|c| c.visible);
        let wanted = if relative {
            AppliedCursor::Hidden
        } else if self.menu_open || self.pointer_over_ui || self.closed.is_some() {
            AppliedCursor::Default
        } else if !host_visible {
            AppliedCursor::Hidden
        } else {
            match self.host_cursor.map(|c| c.shape) {
                Some(id) if self.os_cursors.contains_key(&id) => AppliedCursor::Remote(id),
                _ => AppliedCursor::Default,
            }
        };
        if self.applied == Some(wanted) {
            return;
        }
        match wanted {
            AppliedCursor::Hidden => window.set_cursor_visible(false),
            AppliedCursor::Default => {
                window.set_cursor(CursorIcon::Default);
                window.set_cursor_visible(true);
            }
            AppliedCursor::Remote(id) => {
                window.set_cursor(self.os_cursors[&id].clone());
                window.set_cursor_visible(true);
            }
        }
        self.applied = Some(wanted);
    }

    fn handle_host(&mut self, event_loop: &ActiveEventLoop, msg: HostMsg) {
        match msg {
            HostMsg::SurfaceStarted(info) => {
                tracing::info!(?info, "surface started");
                self.surface = Some(info);
                self.fit_window();
            }
            HostMsg::SurfaceEnded { .. } => self.surface = None,
            HostMsg::CursorShape(shape) => {
                match CustomCursor::from_rgba(
                    shape.rgba.clone(),
                    shape.width as u16,
                    shape.height as u16,
                    shape.hotspot_x as u16,
                    shape.hotspot_y as u16,
                ) {
                    Ok(source) => {
                        self.os_cursors
                            .insert(shape.id, event_loop.create_custom_cursor(source));
                    }
                    Err(e) => tracing::debug!("cursor image rejected: {e}"),
                }
                self.shapes.insert(shape.id, shape);
                self.applied = None;
                self.update_cursor();
            }
            HostMsg::CursorState(state) => {
                self.host_cursor = Some(state);
                self.update_cursor();
                self.request_redraw();
            }
            HostMsg::Notice { text } => {
                tracing::info!("host: {text}");
                self.notices.push((text, Instant::now()));
                self.request_redraw();
            }
            HostMsg::Rejected { reason } => self.closed = Some(reason),
            HostMsg::Clipboard { text } => {
                if let Some(gfx) = &mut self.gfx {
                    gfx.clipboard.set_text(text.clone());
                }
                self.clipboard_last = Some(text);
            }
            _ => {}
        }
    }

    /// Size the window to the remote surface the first time we learn it.
    fn fit_window(&mut self) {
        if self.sized_window || self.in_force.fullscreen {
            return;
        }
        let (Some(window), Some(surface)) = (self.window(), self.surface.as_ref()) else {
            return;
        };
        let (mut w, mut h) = (surface.width as f64, surface.height as f64);
        if let Some(monitor) = window.current_monitor() {
            let m = monitor.size();
            let scale = ((m.width as f64 * 0.9) / w)
                .min((m.height as f64 * 0.9) / h)
                .min(1.0);
            w *= scale;
            h *= scale;
        }
        let size = PhysicalSize::new(w.round() as u32, h.round() as u32);
        // Applied immediately on some platforms (Wayland), without a Resized
        // event.
        if let Some(size) = window.request_inner_size(size)
            && let Some(gfx) = &mut self.gfx
        {
            gfx.renderer.resize(size.width, size.height);
            gfx.window.request_redraw();
        }
        self.sized_window = true;
    }

    fn handle_key(&mut self, event: &KeyEvent) {
        let PhysicalKey::Code(code) = event.physical_key else {
            return;
        };
        let pressed = event.state == ElementState::Pressed;
        if pressed && let Some(command) = self.shortcuts.find(self.modifiers, code) {
            if !event.repeat {
                self.run_command(command);
            }
            return;
        }
        if !self.forwarding() || event.repeat {
            return;
        }
        let Some(usage) = hid_usage(code) else {
            return;
        };
        if pressed {
            self.keys_down.insert(usage);
        } else if !self.keys_down.remove(&usage) {
            // Pressed before the menu closed or focus arrived.
            return;
        }
        self.session.input(InputEvent::Key {
            usage,
            down: pressed,
        });
    }

    /// Keys taken by the Windows keyboard hook while forwarding.
    #[cfg(windows)]
    fn handle_hook_keys(&mut self) {
        let Some(hook) = &self.key_hook else { return };
        for (usage, down) in hook.drain() {
            if down
                && !self.keys_down.contains(&usage)
                && let Some(command) = self.shortcuts.find_hid(&self.keys_down, usage)
            {
                self.run_command(command);
                if command == Command::Menu {
                    break;
                }
                continue;
            }
            if !self.forwarding() {
                continue;
            }
            if down {
                if !self.keys_down.insert(usage) {
                    continue; // auto-repeat; the host repeats on its own
                }
            } else if !self.keys_down.remove(&usage) {
                continue;
            }
            self.session.input(InputEvent::Key { usage, down });
        }
    }

    fn run_command(&mut self, command: Command) {
        match command {
            Command::Menu => {
                let open = !self.menu_open;
                self.set_menu(open);
            }
            Command::Fullscreen => {
                let on = !self.in_force.fullscreen;
                self.edit(|h| h.fullscreen = Some(on));
            }
            Command::Stats => {
                let on = !self.in_force.show_stats;
                self.edit(|h| h.show_stats = Some(on));
            }
            Command::Disconnect => self.apply(Action::Disconnect),
            Command::NextDisplay => self.cycle_display(1),
            Command::PreviousDisplay => self.cycle_display(-1),
        }
    }

    /// Switch to the next (`step` 1) or previous (-1) host display.
    /// The mode of the monitor this window is on.
    fn monitor_mode(&self) -> Option<VirtualMode> {
        let monitor = self.window()?.current_monitor()?;
        let size = monitor.size();
        Some(VirtualMode {
            width: size.width,
            height: size.height,
            refresh_hz: monitor
                .refresh_rate_millihertz()
                .map_or(60, |mhz| (mhz + 500) / 1000),
        })
    }

    fn cycle_display(&mut self, step: isize) {
        let n = self.displays.len();
        if n < 2 {
            self.notices
                .push(("The host has only one display".into(), Instant::now()));
            self.request_redraw();
            return;
        }
        let current = self
            .display
            .or_else(|| self.displays.iter().find(|d| d.primary).map(|d| d.id));
        let i = self
            .displays
            .iter()
            .position(|d| Some(d.id) == current)
            .unwrap_or(0);
        let next = &self.displays[(i as isize + step).rem_euclid(n as isize) as usize];
        let (id, name) = (next.id, next.name.clone());
        self.notices
            .push((format!("Display: {name}"), Instant::now()));
        self.apply(Action::SelectDisplay(id));
    }

    fn handle_pointer_moved(&mut self, position: PhysicalPosition<f64>) {
        self.pointer = Some(position);
        let over_ui = self.egui_ctx.is_pointer_over_egui();
        if over_ui != self.pointer_over_ui {
            self.pointer_over_ui = over_ui;
            self.update_cursor();
        }
        if !self.forwarding() || self.relative() {
            return;
        }
        if let Some((x, y)) = self.to_surface(position)
            && self.last_move != Some((x, y))
        {
            self.last_move = Some((x, y));
            self.session.input(InputEvent::MouseMove {
                surface: self.surface.as_ref().map_or(0, |s| s.id),
                x,
                y,
            });
        }
    }

    fn handle_button(&mut self, button: winit::event::MouseButton, state: ElementState) {
        let button = match button {
            winit::event::MouseButton::Left => MouseButton::Left,
            winit::event::MouseButton::Right => MouseButton::Right,
            winit::event::MouseButton::Middle => MouseButton::Middle,
            winit::event::MouseButton::Back => MouseButton::Back,
            winit::event::MouseButton::Forward => MouseButton::Forward,
            winit::event::MouseButton::Other(_) => return,
        };
        let down = state == ElementState::Pressed;
        if down {
            if !self.forwarding() {
                return;
            }
            // Make sure the host's pointer is where the click happens.
            if !self.relative()
                && let Some(p) = self.pointer
            {
                self.last_move = None;
                self.handle_pointer_moved(p);
            }
            self.buttons_down.insert(button);
        } else if !self.buttons_down.remove(&button) {
            return;
        }
        self.session.input(InputEvent::MouseButton { button, down });
    }

    fn handle_wheel(&mut self, delta: MouseScrollDelta) {
        if !self.forwarding() {
            return;
        }
        let (dx, dy) = match delta {
            MouseScrollDelta::LineDelta(x, y) => ((x * 120.0) as i32, (y * 120.0) as i32),
            MouseScrollDelta::PixelDelta(p) => {
                // Roughly 50 px per wheel notch.
                self.wheel_px.0 += p.x * 120.0 / 50.0;
                self.wheel_px.1 += p.y * 120.0 / 50.0;
                let (x, y) = (self.wheel_px.0.trunc(), self.wheel_px.1.trunc());
                self.wheel_px.0 -= x;
                self.wheel_px.1 -= y;
                (x as i32, y as i32)
            }
        };
        if dx != 0 || dy != 0 {
            self.session.input(InputEvent::MouseWheel { dx, dy });
        }
    }

    /// The overlay shows something that changes without new frames.
    fn overlay_ticks(&self) -> bool {
        self.in_force.show_stats || self.menu_open || !self.notices.is_empty()
    }

    /// Change this host's settings (from shortcuts) and apply them.
    fn edit(&mut self, f: impl FnOnce(&mut crate::config::HostPrefs)) {
        f(self.config.host_mut(&self.options.target));
        self.config_dirty = true;
        self.sync();
    }

    /// Save changed settings.
    fn save(&mut self) {
        if !std::mem::take(&mut self.config_dirty) {
            return;
        }
        if let Some(paths) = &self.options.paths
            && let Err(e) = self.config.save(paths)
        {
            tracing::warn!("cannot save settings: {e:#}");
        }
    }

    /// Apply what changed in this host's settings to the running session.
    fn sync(&mut self) {
        let new = self.config.effective(Some(&self.options.target));
        self.shortcuts = self.config.bindings();
        let old = std::mem::replace(&mut self.in_force, new.clone());
        if old == new {
            return;
        }
        let s = &self.session;
        if old.codec != new.codec {
            let (codecs, auto_codec) = crate::config::codec_choice(&new.codec);
            s.send(ClientMsg::SetCodecs { codecs, auto_codec });
        }
        if old.bitrate_kbps != new.bitrate_kbps {
            s.send(ClientMsg::SetBitrate {
                kbps: new.bitrate_kbps,
            });
        }
        if old.pyrowave_kbps != new.pyrowave_kbps {
            s.send(ClientMsg::SetPyroWaveBitrate {
                kbps: new.pyrowave_kbps,
            });
        }
        if old.max_fps != new.max_fps {
            s.send(ClientMsg::SetMaxFps { fps: new.max_fps });
        }
        if old.resolution != new.resolution {
            self.sized_window = false;
            s.send(ClientMsg::SetResolution {
                resolution: new.resolution,
            });
        }
        if old.virtual_display != new.virtual_display {
            self.sized_window = false;
            s.send(ClientMsg::SetVirtualDisplay {
                mode: new.virtual_display,
            });
        }
        if old.audio_device != new.audio_device {
            s.send(ClientMsg::SetAudioDevice {
                id: new.audio_device.clone(),
            });
        }
        if old.audio != new.audio {
            s.send(ClientMsg::SetAudio { enabled: new.audio });
        }
        if (old.audio, &old.playback_device) != (new.audio, &new.playback_device)
            && let Err(e) = s.set_speaker(new.audio.then(|| new.playback_device.clone()))
        {
            self.notices
                .push((format!("No sound output: {e:#}"), Instant::now()));
        }
        if (old.microphone, &old.microphone_device) != (new.microphone, &new.microphone_device)
            && let Err(e) = s.set_microphone(new.microphone, new.microphone_device.clone())
        {
            self.notices
                .push((format!("Microphone unavailable: {e:#}"), Instant::now()));
        }
        if old.gamepads != new.gamepads {
            s.set_gamepads(new.gamepads);
        }
        if old.fullscreen != new.fullscreen
            && let Some(w) = self.window()
        {
            w.set_fullscreen(new.fullscreen.then_some(Fullscreen::Borderless(None)));
        }
        if old.mouse != new.mouse {
            self.update_cursor();
        }
        self.request_redraw();
    }

    fn draw(&mut self) {
        if let Some(ready) = self.session.frames.take() {
            if let Some(gfx) = &mut self.gfx {
                match ready.picture {
                    crate::decode::Picture::Cpu(frame) => gfx.renderer.upload(&frame),
                    #[cfg(not(target_os = "macos"))]
                    crate::decode::Picture::Gpu(picture) => gfx.renderer.show_gpu(picture),
                }
            }
            let now = Instant::now();
            // Capture on the host to drawing here (clock-synced), or from
            // receipt when the clocks are not synced yet.
            let latency = self
                .session
                .clock
                .lock()
                .since_capture(ready.header.capture_us, now)
                .unwrap_or_else(|| now.duration_since(ready.received));
            self.latency_ms = self.latency_ms * 0.9 + latency.as_secs_f32() * 1000.0 * 0.1;
            self.first_frame.get_or_insert(now);
            self.frames_presented += 1;
            let _ = ready.decoded;
        }
        let viewport = self.viewport();
        let Some(gfx) = &mut self.gfx else { return };
        // Keep an undragged menu centered when the window size changes
        // (Wayland applies our own resizes without a Resized event).
        let size = gfx.window.inner_size();
        if self.menu_window != Some(size) {
            self.menu_window = Some(size);
            if self.menu_pos.is_none() {
                self.menu_placed = false;
            }
        }
        if self.menu_open && !self.menu_placed {
            self.menu_generation += 1;
        }
        let raw_input = gfx.egui.take_egui_input(&gfx.window);
        let ctx = self.egui_ctx.clone();
        let mut actions = Vec::new();
        let state = OverlayState::from(&*self);
        let live = self.live();
        let mut settings = MenuSettings {
            config: &mut self.config,
            panel: &mut self.panel,
            host: &self.options.target,
            live: &live,
        };
        let output = ctx.run_pass(raw_input, |ctx| {
            actions = overlay(ctx, &state, &mut settings);
        });
        if self.menu_open {
            self.menu_placed = true;
        }
        let gfx = self.gfx.as_mut().unwrap();
        if let Err(e) = gfx.renderer.render(viewport, &ctx, output) {
            tracing::error!("render failed: {e:#}");
        }
        for action in actions {
            self.apply(action);
        }
    }

    /// Send local clipboard changes to the host while the window is focused.
    fn poll_clipboard(&mut self) {
        if !self.focused
            || self.closed.is_some()
            || self.clipboard_polled.elapsed() < Duration::from_millis(500)
        {
            return;
        }
        self.clipboard_polled = Instant::now();
        let Some(gfx) = &mut self.gfx else { return };
        let Some(text) = gfx.clipboard.get() else {
            return;
        };
        if text.is_empty() || self.clipboard_last.as_ref() == Some(&text) || text.len() > 1 << 20 {
            return;
        }
        self.clipboard_last = Some(text.clone());
        self.session.send(ClientMsg::Clipboard { text });
    }

    fn apply(&mut self, action: Action) {
        match action {
            Action::OpenMenu => self.set_menu(true),
            Action::CloseMenu => self.set_menu(false),
            Action::SecureAttention => self.session.input(InputEvent::SecureAttention),
            Action::MoveMenuButton(pos) => {
                // Saved with the next settings change or when the session ends.
                self.config.menu_button_pos = Some(pos);
                self.config_dirty = true;
            }
            Action::MenuMoved(pos) => self.menu_pos = Some(pos),
            Action::SettingsChanged => {
                self.config_dirty = true;
                self.sync();
                self.save();
            }
            Action::SelectDisplay(id) => {
                // Streaming a physical display ends the extra display; the
                // host switches straight to the chosen one.
                self.config.host_mut(&self.options.target).virtual_display = None;
                self.in_force.virtual_display = None;
                self.config_dirty = true;
                self.display = Some(id);
                self.sized_window = false;
                self.session.send(ClientMsg::SelectDisplay { id });
            }
            Action::Disconnect => {
                self.session.close();
                self.exit.get_or_insert(ViewerExit::Back);
            }
        }
        self.request_redraw();
    }

    /// What the host offers, for the settings panel.
    fn live(&self) -> crate::settings_ui::Live {
        let display = self
            .display
            .or_else(|| self.displays.iter().find(|d| d.primary).map(|d| d.id));
        crate::settings_ui::Live {
            displays: self
                .displays
                .iter()
                .map(|d| {
                    let primary = if d.primary { ", primary" } else { "" };
                    (
                        d.id,
                        format!(
                            "{} ({}×{} @ {} Hz{primary})",
                            d.name, d.width, d.height, d.refresh_hz
                        ),
                    )
                })
                .collect(),
            display,
            virtual_ok: self.virtual_ok,
            monitor_mode: self.monitor_mode(),
            modes: self
                .displays
                .iter()
                .find(|d| Some(d.id) == display)
                .map(|d| d.modes.clone())
                .unwrap_or_default(),
            host_codecs: self.host_codecs.clone(),
            audio_devices: self
                .audio_devices
                .iter()
                .map(|d| (d.id.clone(), d.name.clone()))
                .collect(),
        }
    }
}

enum Action {
    OpenMenu,
    CloseMenu,
    SecureAttention,
    SelectDisplay(u32),
    MoveMenuButton([f32; 2]),
    MenuMoved([f32; 2]),
    /// The settings panel changed the config.
    SettingsChanged,
    Disconnect,
}

/// Everything the overlay draws, copied out so the UI closure does not
/// borrow the viewer.
struct OverlayState {
    host_name: String,
    shortcut: String,
    menu_open: bool,
    show_stats: bool,
    stats_line: String,
    notices: Vec<String>,
    closed: Option<String>,
    remote_cursor: Option<(egui::TextureId, egui::Vec2, egui::Pos2)>,
    menu_button: String,
    menu_button_pos: Option<[f32; 2]>,
    /// Place the menu this frame: where it was dragged, else centered.
    place_menu: Option<Option<[f32; 2]>>,
    menu_generation: u32,
    /// Where the menu is meant to be (where it was dragged).
    menu_center: Option<egui::Pos2>,
}

impl From<&Viewer> for OverlayState {
    fn from(v: &Viewer) -> Self {
        let summary = v.session.stats.lock().summary();
        let s = v.session.stats.lock();
        let stats_line = format!(
            "{} {}×{} · {:.0} fps · {:.1} Mbit/s · encode {:.1} ms · decode {:.1} ms · capture→draw {:.1} ms · RTT {:.1} ms\n{} · {}",
            s.codec,
            s.resolution.0,
            s.resolution.1,
            summary.fps,
            summary.mbps,
            summary.encode_ms,
            summary.decode_ms,
            v.latency_ms,
            v.session.rtt().as_secs_f32() * 1000.0,
            s.decoder,
            v.gfx
                .as_ref()
                .map_or("", |g| g.renderer.adapter_info.as_str()),
        );
        drop(s);
        // Draw the host's cursor ourselves when the local pointer is not
        // showing it: in relative mode, or when the host's own mouse moves
        // it (e.g. onto an extra display) while ours is elsewhere.
        let local_pointer = v.pointer.is_some() && v.focused && !v.relative();
        let remote_cursor = (!local_pointer && v.host_cursor.is_some_and(|c| c.visible))
            .then(|| {
                let c = v.host_cursor?;
                let tex = v.egui_cursors.get(&c.shape)?;
                let shape = v.shapes.get(&c.shape)?;
                let vp = v.viewport()?;
                let surface = v.surface.as_ref()?;
                let ppp = v.egui_ctx.pixels_per_point();
                let sx = vp.w / surface.width as f32;
                let pos = egui::pos2(
                    (vp.x + (c.x as f32 - shape.hotspot_x as f32) * sx) / ppp,
                    (vp.y + (c.y as f32 - shape.hotspot_y as f32) * sx) / ppp,
                );
                Some((
                    tex.id(),
                    egui::vec2(shape.width as f32, shape.height as f32) * sx / ppp,
                    pos,
                ))
            })
            .flatten();
        OverlayState {
            host_name: v.host_name.clone(),
            shortcut: v.shortcuts.menu().to_string(),
            menu_open: v.menu_open,
            show_stats: v.in_force.show_stats,
            stats_line,
            notices: v
                .notices
                .iter()
                .filter(|(_, at)| at.elapsed() < Duration::from_secs(5))
                .map(|(t, _)| t.clone())
                .collect(),
            closed: v.closed.clone(),
            remote_cursor,
            menu_button: v.in_force.menu_button.clone(),
            menu_button_pos: v.config.menu_button_pos,
            place_menu: (!v.menu_placed).then_some(v.menu_pos),
            menu_generation: v.menu_generation,
            menu_center: v.menu_pos.and_then(|[fx, fy]| {
                let size = v
                    .window()?
                    .inner_size()
                    .to_logical::<f32>(v.egui_ctx.pixels_per_point() as f64);
                Some(egui::pos2(fx * size.width, fy * size.height))
            }),
        }
    }
}

/// The settings the menu edits: this host's, in `config`.
struct MenuSettings<'a> {
    config: &'a mut ClientConfig,
    panel: &'a mut crate::settings_ui::Panel,
    host: &'a str,
    live: &'a crate::settings_ui::Live,
}

fn overlay(ctx: &egui::Context, s: &OverlayState, settings: &mut MenuSettings) -> Vec<Action> {
    use crate::theme::{self, MUTED};
    let mut actions = Vec::new();

    if let Some((tex, size, pos)) = s.remote_cursor {
        ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::unique("cursor"),
        ))
        .image(
            tex,
            egui::Rect::from_min_size(pos, size),
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            egui::Color32::WHITE,
        );
    }

    if s.show_stats {
        egui::Area::new(egui::Id::unique("stats"))
            .anchor(egui::Align2::LEFT_TOP, egui::vec2(8.0, 8.0))
            .interactable(false)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .fill(egui::Color32::from_black_alpha(170))
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new(&s.stats_line).monospace().size(11.0));
                    });
            });
    }

    if !s.notices.is_empty() {
        egui::Area::new(egui::Id::unique("notices"))
            .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 12.0))
            .interactable(false)
            .show(ctx, |ui| {
                for n in &s.notices {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.label(n);
                    });
                }
            });
    }

    let screen = ctx.content_rect();
    if let Some(reason) = &s.closed {
        egui::Window::new("Disconnected")
            .title_bar(false)
            .frame(theme::dialog_frame())
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.set_width((screen.width() - 80.0).clamp(240.0, 420.0));
                if theme::title_bar(ui, "Disconnected", Some(&s.host_name)) {
                    actions.push(Action::Disconnect);
                }
                ui.add_space(6.0);
                theme::note(ui, reason);
                ui.add_space(12.0);
                if ui.add(theme::primary_button("Close")).clicked() {
                    actions.push(Action::Disconnect);
                }
            });
        return actions;
    }

    // The always-available way back out: a button with the shortcut that
    // can be dragged anywhere (or hidden, leaving the shortcut).
    if !s.menu_open {
        if s.menu_button != "hidden" {
            menu_button(ctx, s, &mut actions);
        }
        return actions;
    }

    // Opens centered, or where it was dragged; can be dragged.
    let to_screen =
        |[fx, fy]: [f32; 2]| screen.min + egui::vec2(fx * screen.width(), fy * screen.height());
    // A fresh window id whenever it is placed: egui's window dragging would
    // otherwise restore the stored position over `current_pos`.
    let menu_id = egui::Id::unique("oriel-menu").with(s.menu_generation);
    let width = (screen.width() - 60.0).clamp(300.0, 580.0);
    let height = (screen.height() - 270.0).clamp(140.0, 340.0);
    let mut menu = egui::Window::new("oriel")
        .id(menu_id)
        .title_bar(false)
        .frame(theme::dialog_frame())
        .collapsible(false)
        .resizable(false)
        .pivot(egui::Align2::CENTER_CENTER)
        .default_pos(screen.center());
    if let Some(pos) = s.place_menu {
        menu = menu.current_pos(pos.map_or(screen.center(), to_screen));
    }
    let shown = menu.show(ctx, |ui| {
        ui.set_width(width);
        if theme::title_bar(
            ui,
            &s.host_name,
            Some(&format!("Connected · {} returns", s.shortcut)),
        ) {
            actions.push(Action::CloseMenu);
        }
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new(&s.stats_line)
                .monospace()
                .size(10.5)
                .color(MUTED),
        );
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui
                .button("Send Ctrl+Alt+Del")
                .on_hover_text("Reach the lock screen, task manager or sign-in on the host")
                .clicked()
            {
                actions.push(Action::SecureAttention);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add(theme::danger_button("Disconnect")).clicked() {
                    actions.push(Action::Disconnect);
                }
            });
        });
        ui.add_space(6.0);
        ui.separator();
        ui.add_space(4.0);
        let outcome = settings.panel.show(
            ui,
            settings.config,
            Some(settings.host),
            Some(settings.live),
            egui::vec2(width, height),
        );
        for action in outcome.actions {
            match action {
                crate::settings_ui::LiveAction::SelectDisplay(id) => {
                    actions.push(Action::SelectDisplay(id))
                }
            }
        }
        if outcome.changed {
            actions.push(Action::SettingsChanged);
        }
    });
    if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        actions.push(Action::CloseMenu);
    }
    // Remember where it was dragged (the frame does the dragging, so
    // compare with where it was put instead of asking for a drag).
    if let Some(shown) = shown
        && s.place_menu.is_none()
        && (shown.response.rect.center() - s.menu_center.unwrap_or(screen.center())).length() > 2.0
    {
        let c = shown.response.rect.center();
        actions.push(Action::MenuMoved([
            (c.x - screen.min.x) / screen.width().max(1.0),
            (c.y - screen.min.y) / screen.height().max(1.0),
        ]));
    }
    actions
}

fn menu_button(ctx: &egui::Context, s: &OverlayState, actions: &mut Vec<Action>) {
    let screen = ctx.content_rect();
    let area = egui::Area::new(egui::Id::unique("menu-button")).order(egui::Order::Foreground);
    let area = match s.menu_button_pos {
        Some([fx, fy]) => area
            .pivot(egui::Align2::CENTER_CENTER)
            .fixed_pos(screen.min + egui::vec2(fx * screen.width(), fy * screen.height())),
        None => area.anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-8.0, -8.0)),
    };
    area.show(ctx, |ui| {
        let text = match s.menu_button.as_str() {
            "shortcut" => s.shortcut.clone(),
            "logo" => String::new(),
            _ => format!("Menu  ·  {}", s.shortcut),
        };
        let logo = matches!(s.menu_button.as_str(), "full" | "logo");
        let galley = ui.painter().layout_no_wrap(
            text.clone(),
            egui::FontId::proportional(13.0),
            egui::Color32::WHITE,
        );
        let (h, pad, logo_size) = (30.0, 12.0, 18.0);
        let gap = if logo && !text.is_empty() { 8.0 } else { 0.0 };
        let w = if text.is_empty() {
            h
        } else {
            pad * 2.0 + if logo { logo_size } else { 0.0 } + gap + galley.size().x
        };
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::click_and_drag());
        let alpha = if response.hovered() || response.dragged() {
            235
        } else {
            190
        };
        let painter = ui.painter();
        painter.rect_filled(
            rect,
            h / 2.0,
            egui::Color32::from_rgba_unmultiplied(42, 47, 69, alpha),
        );
        let mut x = if text.is_empty() {
            rect.center().x - logo_size / 2.0
        } else {
            rect.left() + pad
        };
        if logo {
            crate::theme::paint_logo(
                painter,
                egui::Rect::from_min_size(
                    egui::pos2(x, rect.center().y - logo_size / 2.0),
                    egui::vec2(logo_size, logo_size),
                ),
            );
            x += logo_size + gap;
        }
        if !text.is_empty() {
            let y = rect.center().y - galley.size().y / 2.0;
            painter.galley(egui::pos2(x, y), galley, egui::Color32::WHITE);
        }
        let response = response.on_hover_text(format!("Menu ({}) · drag to move", s.shortcut));
        if response.dragged() {
            let half = egui::vec2(w / 2.0, h / 2.0);
            let c =
                (rect.center() + response.drag_delta()).clamp(screen.min + half, screen.max - half);
            let fx = (c.x - screen.min.x) / screen.width().max(1.0);
            let fy = (c.y - screen.min.y) / screen.height().max(1.0);
            actions.push(Action::MoveMenuButton([fx, fy]));
        } else if response.clicked() {
            actions.push(Action::OpenMenu);
        }
    });
}

impl ApplicationHandler<UiEvent> for Viewer {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gfx.is_some() {
            return;
        }
        let attrs = crate::app_window(Window::default_attributes())
            .with_title(format!("{} — oriel", self.host_name))
            .with_window_icon(crate::launcher::icon())
            .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 720.0));
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                tracing::error!("cannot create window: {e}");
                event_loop.exit();
                return;
            }
        };
        if self.in_force.fullscreen {
            window.set_fullscreen(Some(Fullscreen::Borderless(None)));
        }
        let renderer = match Renderer::new(window.clone()) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("cannot initialise rendering: {e:#}");
                event_loop.exit();
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
        use winit::raw_window_handle::HasDisplayHandle;
        let display = window.display_handle().ok().map(|h| h.as_raw());
        // SAFETY: the display outlives the clipboard (both dropped in `exiting`).
        let clipboard = unsafe { egui_winit::clipboard::Clipboard::new(display) };
        #[cfg(not(target_os = "macos"))]
        if self.options.hardware_decode {
            *self.session.interop.lock() = renderer.interop.clone();
        }
        self.gfx = Some(Gfx {
            #[cfg(target_os = "linux")]
            inhibitor: crate::inhibit::ShortcutInhibitor::new(&window),
            window,
            renderer,
            egui,
            clipboard,
        });
        #[cfg(windows)]
        {
            use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
            let window = &self.gfx.as_ref().unwrap().window;
            if let Ok(handle) = window.window_handle()
                && let RawWindowHandle::Win32(h) = handle.as_raw()
                && let Some(sink) = self.sink.clone()
            {
                self.key_hook = crate::winkeys::KeyboardHook::install(
                    windows::Win32::Foundation::HWND(h.hwnd.get() as *mut _),
                    move || sink(UiEvent::HookKeys),
                );
            }
        }
        self.fit_window();
        self.update_cursor();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(gfx) = &mut self.gfx else { return };
        let response = gfx.egui.on_window_event(&gfx.window, &event);
        // egui-winit asks for a repaint on nearly every event (including
        // redraws themselves). Video frames drive redraws; the overlay only
        // needs extra ones while the menu is open or the pointer is on it.
        let overlay_active = self.menu_open || self.pointer_over_ui || self.closed.is_some();
        let pointer_only = matches!(event, WindowEvent::CursorMoved { .. });
        if response.repaint
            && !matches!(event, WindowEvent::RedrawRequested)
            && (overlay_active || !pointer_only)
        {
            gfx.window.request_redraw();
        }
        match event {
            WindowEvent::CloseRequested => {
                self.session.close();
                self.exit.get_or_insert(ViewerExit::Closed);
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                gfx.renderer.resize(size.width, size.height);
                gfx.window.request_redraw();
            }
            WindowEvent::RedrawRequested => self.draw(),
            WindowEvent::Focused(focused) => {
                self.focused = focused;
                self.session
                    .focused
                    .store(focused, std::sync::atomic::Ordering::Relaxed);
                if !focused {
                    self.release_all();
                }
                self.update_cursor();
            }
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            WindowEvent::KeyboardInput { event, .. } => self.handle_key(&event),
            WindowEvent::CursorMoved { position, .. } => self.handle_pointer_moved(position),
            WindowEvent::CursorLeft { .. } => self.pointer = None,
            WindowEvent::MouseInput { state, button, .. } => {
                if !(response.consumed && state == ElementState::Pressed) {
                    self.handle_button(button, state);
                }
            }
            WindowEvent::MouseWheel { delta, .. } if !response.consumed => {
                self.handle_wheel(delta);
            }
            _ => {}
        }
    }

    fn device_event(&mut self, _event_loop: &ActiveEventLoop, _id: DeviceId, event: DeviceEvent) {
        if let DeviceEvent::MouseMotion { delta } = event
            && self.relative()
        {
            self.motion.0 += delta.0;
            self.motion.1 += delta.1;
            let (dx, dy) = (self.motion.0.trunc(), self.motion.1.trunc());
            self.motion.0 -= dx;
            self.motion.1 -= dy;
            if dx != 0.0 || dy != 0.0 {
                self.session.input(InputEvent::MouseMotion {
                    dx: dx as i32,
                    dy: dy as i32,
                });
            }
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UiEvent) {
        match event {
            UiEvent::FrameReady => self.request_redraw(),
            UiEvent::HookKeys => {
                #[cfg(windows)]
                self.handle_hook_keys();
            }
            UiEvent::Host(msg) => self.handle_host(event_loop, msg),
            UiEvent::CursorPosition { x, y } => {
                if let Some(c) = &mut self.host_cursor {
                    c.x = x;
                    c.y = y;
                    if self.relative() {
                        self.request_redraw();
                    }
                }
            }
            UiEvent::Closed { reason, retry } => {
                tracing::info!("session closed: {reason}");
                self.release_all();
                if self.exit.is_none() {
                    // Hand back to the launcher, which reconnects or shows
                    // the reason.
                    self.exit = Some(ViewerExit::Lost { reason, retry });
                }
                event_loop.exit();
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.exit.is_some() {
            event_loop.exit();
            return;
        }
        // Cursor images for drawing the remote cursor ourselves.
        for (id, shape) in &self.shapes {
            if !self.egui_cursors.contains_key(id) {
                let image = egui::ColorImage::from_rgba_unmultiplied(
                    [shape.width as usize, shape.height as usize],
                    &shape.rgba,
                );
                let handle = self.egui_ctx.load_texture(
                    format!("cursor-{id}"),
                    image,
                    egui::TextureOptions::NEAREST,
                );
                self.egui_cursors.insert(*id, handle);
            }
        }
        if let Some((path, after)) = &self.options.screenshot
            && let Some(first) = self.first_frame
            && first.elapsed() >= *after
            && let Some(gfx) = &mut self.gfx
        {
            if gfx.renderer.screenshot.is_none() && !self.screenshot_taken {
                gfx.renderer.screenshot = Some(path.clone());
                self.screenshot_taken = true;
                gfx.window.request_redraw();
            } else if self.screenshot_taken && gfx.renderer.screenshot.is_none() {
                tracing::info!(frames = self.frames_presented, "screenshot done, exiting");
                self.session.close();
                self.exit.get_or_insert(ViewerExit::Closed);
                event_loop.exit();
                return;
            }
        }
        self.notices
            .retain(|(_, at)| at.elapsed() < Duration::from_secs(5));
        // Refresh statistics and expire notices even when no video arrives.
        self.poll_clipboard();
        #[cfg(target_os = "linux")]
        if let Some(i) = self.gfx.as_mut().and_then(|g| g.inhibitor.as_mut()) {
            let active = i.active();
            if active != self.shortcuts_inhibited {
                tracing::info!(active, "compositor shortcuts passed to the host");
                self.shortcuts_inhibited = active;
            }
        }
        // Wake for the clipboard while focused; redraw only for overlays.
        let periodic = self.overlay_ticks() || (self.focused && self.closed.is_none());
        event_loop.set_control_flow(if periodic {
            ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(250))
        } else {
            ControlFlow::Wait
        });
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        #[cfg(windows)]
        {
            self.key_hook = None;
        }
        // The surface must go before the display connection does.
        self.egui_cursors.clear();
        #[cfg(target_os = "linux")]
        if let Some(gfx) = &mut self.gfx {
            gfx.inhibitor = None;
        }
        self.gfx = None;
    }

    fn new_events(&mut self, _event_loop: &ActiveEventLoop, cause: StartCause) {
        if !matches!(cause, StartCause::ResumeTimeReached { .. }) {
            return;
        }
        // Redraw once more after the last notice expires, to remove it.
        let notices = self.notices.len();
        self.notices
            .retain(|(_, at)| at.elapsed() < Duration::from_secs(5));
        if self.overlay_ticks() || self.notices.len() != notices {
            self.request_redraw();
        }
    }
}
