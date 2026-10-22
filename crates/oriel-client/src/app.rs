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
    SurfaceInfo,
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
    connection,
    keymap::{Shortcut, hid_usage},
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
    pub target: String,
    pub settings: StreamSettings,
    pub hardware_decode: bool,
    pub menu_shortcut: Shortcut,
    pub mouse_mode: MouseMode,
    pub fullscreen: bool,
    pub show_stats: bool,
    /// Debugging: save a screenshot after this long, then exit.
    pub screenshot: Option<(PathBuf, Duration)>,
}

pub fn run(
    rt: tokio::runtime::Runtime,
    paths: oriel_net::Paths,
    options: ViewerOptions,
) -> Result<()> {
    let connected = rt.block_on(connection::connect(&paths, &options.target))?;
    tracing::info!(host = %connected.host_name, "connected");
    let event_loop = EventLoop::<UiEvent>::with_user_event().build()?;
    let proxy = Mutex::new(event_loop.create_proxy());
    let sink: session::Sink = Arc::new(move |ev| {
        let _ = proxy.lock().send_event(ev);
    });
    let session = session::start(
        rt.handle(),
        connected.conn.clone(),
        connected.ctl,
        options.settings.clone(),
        options.hardware_decode,
        sink,
    );
    let mut viewer = Viewer::new(session.clone(), connected.host_name, options);
    event_loop.run_app(&mut viewer)?;
    session.close();
    rt.block_on(async {
        tokio::time::sleep(Duration::from_millis(150)).await;
        connected.endpoint.wait_idle().await;
    });
    rt.shutdown_timeout(Duration::from_millis(500));
    Ok(())
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
    show_stats: bool,
    mouse_mode: MouseMode,
    bitrate_mbps: u32,
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
    quit: bool,
}

impl Viewer {
    fn new(session: Session, host_name: String, options: ViewerOptions) -> Self {
        let egui_ctx = egui::Context::default();
        egui_ctx.set_visuals(egui::Visuals::dark());
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
            show_stats: options.show_stats,
            mouse_mode: options.mouse_mode,
            bitrate_mbps: (options.settings.bitrate_kbps / 1000).max(1),
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
            quit: false,
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
        let wanted = match self.mouse_mode {
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
        self.release_all();
        self.update_cursor();
        self.request_redraw();
    }

    fn update_cursor(&mut self) {
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
            _ => {}
        }
    }

    /// Size the window to the remote surface the first time we learn it.
    fn fit_window(&mut self) {
        if self.sized_window || self.options.fullscreen {
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
        if pressed && self.options.menu_shortcut.matches(self.modifiers, code) {
            let open = !self.menu_open;
            self.set_menu(open);
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

    fn draw(&mut self) {
        if let Some(ready) = self.session.frames.take() {
            if let Some(gfx) = &mut self.gfx {
                gfx.renderer.upload(&ready.frame);
            }
            let now = Instant::now();
            // Network+decode latency of this frame, from when its first byte
            // was received (the host clock is not comparable).
            let local = now.duration_since(ready.received).as_secs_f32() * 1000.0;
            self.latency_ms = self.latency_ms * 0.9 + local * 0.1;
            self.first_frame.get_or_insert(now);
            self.frames_presented += 1;
            let _ = ready.decoded;
        }
        let viewport = self.viewport();
        let Some(gfx) = &mut self.gfx else { return };
        let raw_input = gfx.egui.take_egui_input(&gfx.window);
        let ctx = self.egui_ctx.clone();
        let mut actions = Vec::new();
        let output = ctx.run_pass(raw_input, |ctx| {
            actions = overlay(ctx, &OverlayState::from(&*self), viewport);
        });
        let gfx = self.gfx.as_mut().unwrap();
        if let Err(e) = gfx.renderer.render(viewport, &ctx, output) {
            tracing::error!("render failed: {e:#}");
        }
        for action in actions {
            self.apply(action);
        }
    }

    fn apply(&mut self, action: Action) {
        match action {
            Action::OpenMenu => self.set_menu(true),
            Action::CloseMenu => self.set_menu(false),
            Action::SetMouseMode(mode) => {
                self.mouse_mode = mode;
                self.update_cursor();
            }
            Action::ToggleStats => self.show_stats = !self.show_stats,
            Action::ToggleFullscreen => {
                if let Some(w) = self.window() {
                    w.set_fullscreen(match w.fullscreen() {
                        Some(_) => None,
                        None => Some(Fullscreen::Borderless(None)),
                    });
                }
            }
            Action::SetBitrate(mbps) => {
                self.bitrate_mbps = mbps;
                self.session
                    .send(ClientMsg::SetBitrate { kbps: mbps * 1000 });
            }
            Action::SecureAttention => self.session.input(InputEvent::SecureAttention),
            Action::Disconnect => {
                self.session.close();
                self.closed = Some("disconnected".into());
                self.quit = true;
            }
        }
        self.request_redraw();
    }
}

enum Action {
    OpenMenu,
    CloseMenu,
    SetMouseMode(MouseMode),
    ToggleStats,
    ToggleFullscreen,
    SetBitrate(u32),
    SecureAttention,
    Disconnect,
}

/// Everything the overlay draws, copied out so the UI closure does not
/// borrow the viewer.
struct OverlayState {
    host_name: String,
    shortcut: String,
    menu_open: bool,
    show_stats: bool,
    mouse_mode: MouseMode,
    bitrate_mbps: u32,
    stats_line: String,
    notices: Vec<String>,
    closed: Option<String>,
    remote_cursor: Option<(egui::TextureId, egui::Vec2, egui::Pos2)>,
}

impl From<&Viewer> for OverlayState {
    fn from(v: &Viewer) -> Self {
        let summary = v.session.stats.lock().summary();
        let s = v.session.stats.lock();
        let stats_line = format!(
            "{} {}×{} · {:.0} fps · {:.1} Mbit/s · encode {:.1} ms · decode {:.1} ms · recv→decoded {:.1} ms · RTT {:.1} ms\n{} · {}",
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
        let remote_cursor = (v.relative() && v.host_cursor.is_some_and(|c| c.visible))
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
            shortcut: v.options.menu_shortcut.to_string(),
            menu_open: v.menu_open,
            show_stats: v.show_stats,
            mouse_mode: v.mouse_mode,
            bitrate_mbps: v.bitrate_mbps,
            stats_line,
            notices: v
                .notices
                .iter()
                .filter(|(_, at)| at.elapsed() < Duration::from_secs(5))
                .map(|(t, _)| t.clone())
                .collect(),
            closed: v.closed.clone(),
            remote_cursor,
        }
    }
}

fn overlay(ctx: &egui::Context, s: &OverlayState, viewport: Option<Viewport>) -> Vec<Action> {
    let mut actions = Vec::new();
    let _ = viewport;

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

    if let Some(reason) = &s.closed {
        egui::Window::new("Disconnected")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.label(format!("{}: {reason}", s.host_name));
                if ui.button("Close").clicked() {
                    actions.push(Action::Disconnect);
                }
            });
        return actions;
    }

    // The always-available way back out: a small button with the shortcut.
    if !s.menu_open {
        egui::Area::new(egui::Id::unique("menu-button"))
            .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-8.0, -8.0))
            .show(ctx, |ui| {
                let hint = format!("Menu  {}", s.shortcut);
                let button = egui::Button::new(egui::RichText::new(hint).size(11.0))
                    .fill(egui::Color32::from_black_alpha(140));
                if ui.add(button).clicked() {
                    actions.push(Action::OpenMenu);
                }
            });
        return actions;
    }

    egui::Window::new(format!("oriel — {}", s.host_name))
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            ui.label(egui::RichText::new(&s.stats_line).monospace().size(11.0));
            ui.separator();
            ui.horizontal(|ui| {
                ui.label("Mouse:");
                for (mode, label) in [
                    (MouseMode::Auto, "Auto"),
                    (MouseMode::Absolute, "Desktop"),
                    (MouseMode::Relative, "Captured (games)"),
                ] {
                    if ui.selectable_label(s.mouse_mode == mode, label).clicked() {
                        actions.push(Action::SetMouseMode(mode));
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.label("Bitrate:");
                let mut mbps = s.bitrate_mbps;
                let slider = ui.add(
                    egui::Slider::new(&mut mbps, 2..=500)
                        .logarithmic(true)
                        .suffix(" Mbit/s"),
                );
                if slider.drag_stopped() || (slider.changed() && !slider.dragged()) {
                    actions.push(Action::SetBitrate(mbps));
                }
            });
            ui.horizontal(|ui| {
                if ui.button("Fullscreen").clicked() {
                    actions.push(Action::ToggleFullscreen);
                }
                if ui
                    .button(if s.show_stats {
                        "Hide statistics"
                    } else {
                        "Show statistics"
                    })
                    .clicked()
                {
                    actions.push(Action::ToggleStats);
                }
                if ui.button("Send Ctrl+Alt+Del").clicked() {
                    actions.push(Action::SecureAttention);
                }
            });
            ui.separator();
            ui.horizontal(|ui| {
                if ui
                    .button(format!("Back to {} ({})", s.host_name, s.shortcut))
                    .clicked()
                {
                    actions.push(Action::CloseMenu);
                }
                if ui.button("Disconnect").clicked() {
                    actions.push(Action::Disconnect);
                }
            });
        });
    actions
}

impl ApplicationHandler<UiEvent> for Viewer {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gfx.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title(format!("{} — oriel", self.host_name))
            .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 720.0));
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                tracing::error!("cannot create window: {e}");
                event_loop.exit();
                return;
            }
        };
        if self.options.fullscreen {
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
        self.gfx = Some(Gfx {
            window,
            renderer,
            egui,
        });
        self.fit_window();
        self.update_cursor();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(gfx) = &mut self.gfx else { return };
        let response = gfx.egui.on_window_event(&gfx.window, &event);
        if response.repaint {
            gfx.window.request_redraw();
        }
        match event {
            WindowEvent::CloseRequested => {
                self.session.close();
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                gfx.renderer.resize(size.width, size.height);
                gfx.window.request_redraw();
            }
            WindowEvent::RedrawRequested => self.draw(),
            WindowEvent::Focused(focused) => {
                self.focused = focused;
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
            WindowEvent::MouseWheel { delta, .. } => {
                if !response.consumed {
                    self.handle_wheel(delta);
                }
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
            UiEvent::Closed(reason) => {
                tracing::info!("session closed: {reason}");
                if self.closed.is_none() {
                    self.closed = Some(reason);
                }
                self.release_all();
                self.update_cursor();
                self.request_redraw();
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.quit {
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
        {
            if let Some(gfx) = &mut self.gfx {
                if gfx.renderer.screenshot.is_none() && !self.screenshot_taken {
                    gfx.renderer.screenshot = Some(path.clone());
                    self.screenshot_taken = true;
                    gfx.window.request_redraw();
                } else if self.screenshot_taken && gfx.renderer.screenshot.is_none() {
                    tracing::info!(frames = self.frames_presented, "screenshot done, exiting");
                    self.session.close();
                    event_loop.exit();
                    return;
                }
            }
        }
        self.notices
            .retain(|(_, at)| at.elapsed() < Duration::from_secs(5));
        // Refresh statistics and expire notices even when no video arrives.
        let periodic = self.show_stats || self.menu_open || !self.notices.is_empty();
        event_loop.set_control_flow(if periodic {
            ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(250))
        } else {
            ControlFlow::Wait
        });
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        // The surface must go before the display connection does.
        self.egui_cursors.clear();
        self.gfx = None;
    }

    fn new_events(&mut self, _event_loop: &ActiveEventLoop, cause: StartCause) {
        if matches!(cause, StartCause::ResumeTimeReached { .. }) {
            self.request_redraw();
        }
    }
}
