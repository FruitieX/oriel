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
    config::{ClientConfig, Effective, Resolution},
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
        let (codecs, auto_codec) = offered_codecs(&e.codec);
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
                // This computer's screen is known once the window is.
                resolution: e.resolution.size(None),
                audio_device: e.audio_device.clone(),
                virtual_display: e.virtual_display,
                full_chroma: full_chroma_codecs(e.full_chroma),
                software_encode: e.encoding == "software",
                graphics_capture: e.capture == "graphics",
            },
            hardware_decode: e.decoding != "software",
            config,
            paths,
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
    let _awake = crate::nap::begin("Showing a remote desktop");
    let proxy = Mutex::new(event_loop.create_proxy());
    // One frame notice at a time: the viewer takes the newest frame anyway,
    // and a main thread held up (a hidden window on macOS) must not come
    // back to a backlog of thousands.
    let frame_pending = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let pending = frame_pending.clone();
    let sink: session::Sink = Arc::new(move |ev| {
        if matches!(ev, UiEvent::FrameReady)
            && pending.swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return;
        }
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
    session
        .frames
        .0
        .lock()
        .set_smoothing(crate::playout::Smoothing::parse(&e.smoothing));
    // Hosts before protocol 10 skip this.
    session.send(ClientMsg::SetAutoBitrate {
        enabled: e.auto_bitrate,
    });
    let mut viewer = Viewer::new(session.clone(), connected.host_name, options);
    viewer.frame_pending = frame_pending;
    viewer.sink = Some(sink.clone());
    viewer.displays = connected.displays;
    viewer.host_codecs = connected.codecs;
    viewer.audio_devices = connected.audio_devices;
    viewer.virtual_ok = connected.virtual_display;
    viewer.other_protocol = (connected.host_protocol != oriel_proto::PROTOCOL_VERSION)
        .then_some(connected.host_protocol);
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
    // Android's clipboard goes through the activity instead.
    #[cfg_attr(target_os = "android", allow(dead_code))]
    clipboard: egui_winit::clipboard::Clipboard,
    #[cfg(target_os = "linux")]
    inhibitor: Option<crate::inhibit::ShortcutInhibitor>,
}

struct Viewer {
    /// A frame notice is on its way to the event loop (see `run`).
    frame_pending: Arc<std::sync::atomic::AtomicBool>,
    /// The window cannot be seen (minimized, covered, another Space):
    /// nothing is drawn. On macOS drawing would wait up to a second for a
    /// buffer the window server does not hand out, freezing the app.
    occluded: bool,
    session: Session,
    host_name: String,
    options: ViewerOptions,
    gfx: Option<Gfx>,
    egui_ctx: egui::Context,

    surface: Option<SurfaceInfo>,
    sized_window: bool,
    /// The resolution last asked of the host.
    sent_resolution: Option<(u32, u32)>,
    shapes: HashMap<u64, CursorShape>,
    os_cursors: HashMap<u64, CustomCursor>,
    /// How much the local cursors are scaled: as much as the picture, so
    /// that they match the cursor drawn on the host.
    cursor_scale: f32,
    egui_cursors: HashMap<u64, egui::TextureHandle>,
    host_cursor: Option<CursorState>,
    applied: Option<AppliedCursor>,
    grabbed: bool,

    menu_open: bool,
    /// The menu button's quick actions are showing.
    quick_menu_open: bool,
    /// The settings, and those currently in force (see `sync`).
    config: ClientConfig,
    in_force: Effective,
    config_dirty: bool,
    shortcuts: Bindings,
    panel: crate::settings_ui::Panel,
    focused: bool,
    modifiers: ModifiersState,
    keys_down: HashSet<u16>,
    /// When the held keys were last confirmed to the host.
    keys_held_sent: Instant,
    buttons_down: HashSet<MouseButton>,
    pointer: Option<PhysicalPosition<f64>>,
    last_move: Option<(i32, i32)>,
    /// Touch screens: the local zoom and pan, the gesture in progress, and
    /// touches that belong to the overlay.
    view: crate::touch::View,
    /// Modifiers latched on the touch key row.
    sticky: [bool; 4],
    gestures: crate::touch::Gestures,
    touch_ui: std::collections::HashSet<u64>,
    pointer_over_ui: bool,
    wheel_px: (f64, f64),
    motion: (f64, f64),

    notices: Vec<(String, Instant)>,
    /// The host's last health report.
    host_health: Option<(oriel_proto::HostHealth, Instant)>,
    diagnostics: crate::diagnostics::Diagnostics,
    diagnosed: Instant,
    /// The host's protocol version when it differs from ours.
    other_protocol: Option<u32>,
    /// Sound underruns counted at the last check.
    underruns: u64,
    /// The smoothing buffer at the last check (`None`: off).
    buffer_ms: Option<f32>,
    /// The last frame shown (when, captured when), and how unevenly frames
    /// are shown, ms.
    last_shown: Option<(Instant, u64)>,
    shown_jitter_ms: f32,
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
            frame_pending: Default::default(),
            occluded: false,
            session,
            host_name,
            gfx: None,
            egui_ctx,
            surface: None,
            sized_window: false,
            sent_resolution: options.settings.resolution,
            shapes: HashMap::new(),
            os_cursors: HashMap::new(),
            cursor_scale: 1.0,
            egui_cursors: HashMap::new(),
            host_cursor: None,
            applied: None,
            grabbed: false,
            // Debugging aid for screenshots.
            menu_open: std::env::var_os("ORIEL_DEBUG_MENU").is_some(),
            quick_menu_open: false,
            in_force: options.config.effective(Some(&options.target)),
            shortcuts: options.config.bindings(),
            config: options.config.clone(),
            config_dirty: false,
            panel: crate::settings_ui::Panel::new(true),
            focused: false,
            modifiers: ModifiersState::empty(),
            keys_down: HashSet::new(),
            keys_held_sent: Instant::now(),
            buttons_down: HashSet::new(),
            pointer: None,
            last_move: None,
            // Debugging aid: ORIEL_DEBUG_ZOOM=2 starts zoomed in.
            sticky: [false; 4],
            view: crate::touch::View {
                zoom: std::env::var("ORIEL_DEBUG_ZOOM")
                    .ok()
                    .and_then(|z| z.parse::<f32>().ok())
                    .map_or(1.0, |z| z.clamp(1.0, crate::touch::MAX_ZOOM)),
                offset: (0.0, 0.0),
            },
            gestures: crate::touch::Gestures::new(24.0),
            touch_ui: Default::default(),
            pointer_over_ui: false,
            wheel_px: (0.0, 0.0),
            motion: (0.0, 0.0),
            notices: Vec::new(),
            host_health: None,
            diagnostics: Default::default(),
            diagnosed: Instant::now(),
            other_protocol: None,
            underruns: 0,
            buffer_ms: None,
            last_shown: None,
            shown_jitter_ms: 0.0,
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

    /// Is input currently going to the host? The menu does not stop it:
    /// the pointer goes to the host when not over the menu, and keys (see
    /// `keys_to_host`).
    fn forwarding(&self) -> bool {
        self.closed.is_none() && self.focused
    }

    /// Keys go to the host except while the menu records a shortcut (it
    /// has no text fields) or the quick actions show.
    fn keys_to_host(&self) -> bool {
        self.forwarding() && !(self.menu_open && self.panel.is_recording()) && !self.quick_menu_open
    }

    /// The pointer is over the menu or the menu button.
    fn over_ui(&self) -> bool {
        self.egui_ctx.is_pointer_over_egui()
    }

    /// Capture the pointer and shortcuts for the host (not while the menu
    /// or the quick actions are open, so they can be used, and Alt+Tab
    /// reaches this computer).
    fn captured(&self) -> bool {
        self.forwarding() && !self.menu_open && !self.quick_menu_open
    }

    fn relative(&self) -> bool {
        let wanted = match self.in_force.mouse_mode() {
            MouseMode::Auto => self.host_cursor.is_some_and(|c| c.captured),
            MouseMode::Absolute => false,
            MouseMode::Relative => true,
        };
        wanted && self.captured()
    }

    fn viewport(&self) -> Option<Viewport> {
        let gfx = self.gfx.as_ref()?;
        let video = self
            .surface
            .as_ref()
            .map(|s| (s.width, s.height))
            .or(gfx.renderer.video_size())?;
        let window = self.video_area()?;
        Some(
            self.view
                .apply(letterbox(window, video), (window.0 as f32, window.1 as f32)),
        )
    }

    /// The window area the picture fits into: above the soft keyboard.
    fn video_area(&self) -> Option<(u32, u32)> {
        let gfx = self.gfx.as_ref()?;
        #[allow(unused_mut)]
        let mut size = (gfx.renderer.config.width, gfx.renderer.config.height);
        #[cfg(target_os = "android")]
        {
            let keyboard = crate::android::insets().keyboard as u32;
            size.1 = size.1.saturating_sub(keyboard).max(1);
        }
        Some(size)
    }

    fn set_focused(&mut self, focused: bool) {
        tracing::debug!(focused, "window focus");
        self.focused = focused;
        self.session
            .focused
            .store(focused, std::sync::atomic::Ordering::Relaxed);
        if !focused {
            self.release_all();
        }
        self.update_cursor();
    }

    /// Press and release `usage` with the latched modifiers, then release
    /// the latch.
    fn tap_key(&mut self, usage: u16) {
        let mods: Vec<u16> = STICKY
            .iter()
            .zip(self.sticky)
            .filter(|(_, on)| *on)
            .map(|((_, m), _)| *m)
            .collect();
        for &m in &mods {
            self.session.input(InputEvent::Key {
                usage: m,
                down: true,
            });
        }
        self.session.input(InputEvent::Key { usage, down: true });
        self.session.input(InputEvent::Key { usage, down: false });
        for &m in mods.iter().rev() {
            self.session.input(InputEvent::Key {
                usage: m,
                down: false,
            });
        }
        if !mods.is_empty() {
            self.sticky = [false; 4];
            self.request_redraw();
        }
    }

    /// Touch screens (see `crate::touch`): gestures become mouse input, two
    /// fingers zoom and pan the picture.
    fn handle_touch(&mut self, t: winit::event::Touch) {
        use winit::event::TouchPhase;
        let pos = (t.location.x as f32, t.location.y as f32);
        let now = Instant::now();
        // A touch that starts on the menu button, a warning or the menu
        // belongs to them.
        if t.phase == TouchPhase::Started
            && !self.gestures.active()
            && (self.over_ui() || self.menu_open)
        {
            self.touch_ui.insert(t.id);
        }
        if self.touch_ui.contains(&t.id) {
            if matches!(t.phase, TouchPhase::Ended | TouchPhase::Cancelled) {
                self.touch_ui.remove(&t.id);
            }
            return;
        }
        let actions = match t.phase {
            TouchPhase::Started => self.gestures.down(t.id, pos, now),
            TouchPhase::Moved => self.gestures.moved(t.id, pos),
            TouchPhase::Ended => self.gestures.up(t.id, now),
            TouchPhase::Cancelled => self.gestures.cancel(),
        };
        self.apply_touch(actions);
    }

    fn apply_touch(&mut self, actions: Vec<crate::touch::Action>) {
        use crate::touch::Action as A;
        for action in actions {
            match action {
                A::MoveTo(x, y) => {
                    if !self.forwarding() {
                        continue;
                    }
                    if let Some((x, y)) = self.to_surface(PhysicalPosition::new(x as f64, y as f64))
                    {
                        self.last_move = Some((x, y));
                        self.session.input(InputEvent::MouseMove {
                            surface: self.surface.as_ref().map_or(0, |s| s.id),
                            x,
                            y,
                        });
                    }
                }
                A::Button(button, down) => {
                    if !self.forwarding() {
                        continue;
                    }
                    if down {
                        self.buttons_down.insert(button);
                    } else if !self.buttons_down.remove(&button) {
                        continue;
                    }
                    self.session.input(InputEvent::MouseButton { button, down });
                }
                A::Scroll(dx, dy) => self.handle_wheel(MouseScrollDelta::PixelDelta(
                    PhysicalPosition::new(dx as f64, dy as f64),
                )),
                A::Pinch { mid, moved, scale } => {
                    let Some(window) = self.video_area() else {
                        continue;
                    };
                    let Some(surface) = &self.surface else {
                        continue;
                    };
                    let fit = letterbox(window, (surface.width, surface.height));
                    let was_zoomed = self.view.zoomed();
                    let rest =
                        self.view
                            .pinch(fit, (window.0 as f32, window.1 as f32), mid, moved, scale);
                    // With the whole picture in view, two fingers scroll.
                    if !was_zoomed && !self.view.zoomed() && (scale - 1.0).abs() < 0.02 {
                        self.handle_wheel(MouseScrollDelta::PixelDelta(PhysicalPosition::new(
                            rest.0 as f64,
                            rest.1 as f64,
                        )));
                    }
                    self.request_redraw();
                }
            }
        }
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
            hook.set_active(self.captured());
        }
        #[cfg(target_os = "linux")]
        {
            let forwarding = self.captured();
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
        } else if self.pointer_over_ui || self.closed.is_some() {
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

    /// A host cursor for this window, scaled like the picture.
    fn os_cursor(&self, event_loop: &ActiveEventLoop, shape: &CursorShape) -> Option<CustomCursor> {
        let scaled;
        let shape = if (self.cursor_scale - 1.0).abs() < 0.02 {
            shape
        } else {
            scaled = shape.resized(self.cursor_scale);
            &scaled
        };
        match CustomCursor::from_rgba(
            shape.rgba.clone(),
            shape.width as u16,
            shape.height as u16,
            shape.hotspot_x as u16,
            shape.hotspot_y as u16,
        ) {
            Ok(source) => Some(event_loop.create_custom_cursor(source)),
            Err(e) => {
                tracing::debug!("cursor image rejected: {e}");
                None
            }
        }
    }

    /// Follow the picture's scale (window size, host resolution, zoom).
    fn rescale_cursors(&mut self, event_loop: &ActiveEventLoop) {
        let (Some(vp), Some(surface), Some(window)) =
            (self.viewport(), self.surface.as_ref(), self.window())
        else {
            return;
        };
        // Cursor images are in physical pixels, but in points on macOS.
        let per_pixel = if cfg!(target_os = "macos") {
            window.scale_factor() as f32
        } else {
            1.0
        };
        let scale = (vp.w / surface.width.max(1) as f32 / per_pixel).clamp(0.25, 4.0);
        if (scale - self.cursor_scale).abs() < 0.02 * self.cursor_scale {
            return;
        }
        self.cursor_scale = scale;
        let shapes: Vec<_> = self.shapes.values().cloned().collect();
        self.os_cursors = shapes
            .iter()
            .filter_map(|s| Some((s.id, self.os_cursor(event_loop, s)?)))
            .collect();
        self.applied = None;
        self.update_cursor();
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
                if let Some(cursor) = self.os_cursor(event_loop, &shape) {
                    self.os_cursors.insert(shape.id, cursor);
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
            HostMsg::Health(health) => self.host_health = Some((health, Instant::now())),
            HostMsg::Notice { text } => {
                tracing::info!("host: {text}");
                self.notices.push((text, Instant::now()));
                self.request_redraw();
            }
            HostMsg::Rejected { reason } => self.closed = Some(reason),
            HostMsg::Clipboard { text } => {
                #[cfg(target_os = "android")]
                crate::android::set_clipboard_text(&text);
                #[cfg(not(target_os = "android"))]
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
        // The window stays as it is when the stream follows the screen.
        if self.sized_window
            || self.in_force.fullscreen
            || matches!(self.in_force.resolution, Resolution::Client(_))
        {
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
        // Android's Back opens (or closes) the menu.
        if is_back(event) {
            if event.state == ElementState::Pressed && !event.repeat {
                self.set_menu(!self.menu_open);
            }
            return;
        }
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
        // Esc closes the menu, like a dialog, unless it cancels recording a
        // shortcut or closes a drop-down first.
        if pressed
            && self.menu_open
            && code == winit::keyboard::KeyCode::Escape
            && self.modifiers.is_empty()
            && !self.panel.is_recording()
            && !self.egui_ctx.any_popup_open()
        {
            if !event.repeat {
                self.set_menu(false);
            }
            return;
        }
        if !self.keys_to_host() || event.repeat {
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

    /// Text and keys from Android's soft keyboard.
    #[cfg(target_os = "android")]
    fn handle_platform(&mut self) {
        use crate::android::TextInput;
        // A text field in the menu has it (see `draw`).
        if self.egui_ctx.text_edit_focused() {
            return;
        }
        let input = crate::android::take_input();
        if !self.forwarding() {
            return;
        }
        let tap = |s: &Session, usage: u16| {
            s.input(InputEvent::Key { usage, down: true });
            s.input(InputEvent::Key { usage, down: false });
        };
        for input in input {
            match input {
                // With latched modifiers, letters go as keys (Ctrl+C).
                TextInput::Text(text) if self.sticky.iter().any(|m| *m) => {
                    for c in text.chars() {
                        match ascii_usage(c) {
                            Some(usage) => self.tap_key(usage),
                            None => self.session.input(InputEvent::Text {
                                text: c.to_string(),
                            }),
                        }
                    }
                }
                TextInput::Text(text) => self.session.input(InputEvent::Text { text }),
                TextInput::Delete { before, after } => {
                    for _ in 0..before.min(256) {
                        tap(&self.session, oriel_proto::keys::BACKSPACE);
                    }
                    for _ in 0..after.min(256) {
                        tap(&self.session, 0x4C);
                    }
                }
                TextInput::Key { code, down, .. } => {
                    if let Some(usage) = crate::android::hid_usage(code) {
                        self.session.input(InputEvent::Key { usage, down });
                    }
                }
            }
        }
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
            if !self.keys_to_host() {
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
        if !self.forwarding() || self.relative() || over_ui {
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
            if !self.forwarding() || (!self.relative() && self.over_ui()) {
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
        if !self.forwarding() || (!self.relative() && self.over_ui()) {
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
        self.in_force.show_stats
            || self.menu_open
            || !self.notices.is_empty()
            || (self.in_force.warnings != "off" && !self.diagnostics.warnings().is_empty())
    }

    /// Look for problems that explain a slow stream, once a second.
    /// Ask the host for the resolution the settings want, which for "this
    /// computer's" follows the screen the window is on.
    fn update_resolution(&mut self) {
        let screen = self
            .window()
            .and_then(|w| w.current_monitor())
            .map(|m| (m.size().width, m.size().height));
        let wanted = self.in_force.resolution.size(screen);
        if matches!(self.in_force.resolution, Resolution::Client(_)) && screen.is_none() {
            return;
        }
        if wanted != self.sent_resolution {
            tracing::info!(?wanted, "asking the host for a resolution");
            self.sent_resolution = wanted;
            self.sized_window = false;
            self.session
                .send(ClientMsg::SetResolution { resolution: wanted });
        }
    }

    fn diagnose(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.diagnosed) < Duration::from_secs(1) || self.closed.is_some() {
            return;
        }
        self.update_resolution();
        let elapsed = now.duration_since(self.diagnosed).as_secs_f32();
        self.diagnosed = now;
        let (summary, codec, software, full_chroma, jitter_ms) = {
            let mut s = self.session.stats.lock();
            (
                s.summary(),
                s.codec_id,
                s.decoder.contains("(software)"),
                s.full_chroma,
                s.jitter_ms,
            )
        };
        let (buffer_ms, late) = {
            let mut q = self.session.frames.0.lock();
            let (arrived, late) = q.take_counts();
            let buffer = (q.smoothing() != crate::playout::Smoothing::Off)
                .then(|| q.delay().as_secs_f32() * 1000.0);
            (buffer, late as f32 / arrived.max(1) as f32)
        };
        self.buffer_ms = buffer_ms;
        let underruns = self.session.audio_stats().map_or(0, |a| a.underruns);
        let audio_underruns = underruns.saturating_sub(self.underruns) as f32 / elapsed.max(0.001);
        self.underruns = underruns;
        let inputs = crate::diagnostics::Inputs {
            host: self
                .host_health
                .filter(|(_, at)| at.elapsed() < Duration::from_secs(3))
                .map(|(h, _)| h),
            since_pong: self.session.last_pong.lock().elapsed(),
            rtt_ms: self.session.rtt().as_secs_f32() * 1000.0,
            fps: summary.fps,
            decode_ms: summary.decode_ms,
            encode_ms: summary.encode_ms,
            latency_ms: if summary.fps > 0.0 {
                self.latency_ms
            } else {
                0.0
            },
            codec,
            bitrate_mbps: self.in_force.bitrate_kbps / 1000,
            pyrowave_mbps: self.in_force.pyrowave_kbps / 1000,
            software_decode: software,
            hardware_decode: self
                .session
                .hardware_decode
                .load(std::sync::atomic::Ordering::Relaxed),
            hardware_encode: self.in_force.encoding != "software",
            encoder: self.surface.as_ref().map(|s| s.encoder.clone()),
            full_chroma,
            auto_bitrate: self.in_force.auto_bitrate,
            other_protocol: self.other_protocol,
            jitter_ms,
            buffer_ms,
            late,
            audio_underruns,
        };
        // Debugging aid: pretend the network is slow, lossy and far away.
        let mut inputs = inputs;
        if std::env::var_os("ORIEL_DEBUG_BAD_NETWORK").is_some() {
            inputs.rtt_ms += 100.0;
            if let Some(h) = &mut inputs.host {
                h.loss_permille = 45;
                h.network_limited = true;
                h.sent_fps = h.wanted_fps / 2;
            }
        }
        let before = self.diagnostics.warnings();
        self.diagnostics
            .update(now, crate::diagnostics::detect(&inputs));
        let after = self.diagnostics.warnings();
        if before != after {
            for f in after
                .iter()
                .filter(|w| !before.iter().any(|b| b.issue == w.issue))
            {
                tracing::info!(issue = ?f.issue, "{}", f.detail);
            }
            self.request_redraw();
        }
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
        if old.codec != new.codec
            || old.full_chroma != new.full_chroma
            || old.encoding != new.encoding
        {
            let (codecs, auto_codec) = offered_codecs(&new.codec);
            s.send(ClientMsg::SetCodecs {
                codecs,
                auto_codec,
                full_chroma: full_chroma_codecs(new.full_chroma),
                software_encode: new.encoding == "software",
            });
        }
        if old.capture != new.capture {
            s.send(ClientMsg::SetGraphicsCapture {
                enabled: new.capture == "graphics",
            });
        }
        if old.decoding != new.decoding {
            s.hardware_decode.store(
                new.decoding != "software",
                std::sync::atomic::Ordering::Relaxed,
            );
            // The decoder reopens on the next frame, and asks for a
            // keyframe.
        }
        if old.bitrate_kbps != new.bitrate_kbps {
            s.send(ClientMsg::SetBitrate {
                kbps: new.bitrate_kbps,
            });
        }
        if old.smoothing != new.smoothing {
            s.frames
                .0
                .lock()
                .set_smoothing(crate::playout::Smoothing::parse(&new.smoothing));
        }
        if old.auto_bitrate != new.auto_bitrate {
            s.send(ClientMsg::SetAutoBitrate {
                enabled: new.auto_bitrate,
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
            #[cfg(target_os = "android")]
            crate::android::set_immersive(new.fullscreen);
        }
        if old.mouse != new.mouse {
            self.update_cursor();
        }
        self.update_resolution();
        self.request_redraw();
    }

    fn draw(&mut self) {
        if let Some(ready) = self.session.frames.take(Instant::now()) {
            if let Some(gfx) = &mut self.gfx {
                match ready.picture {
                    crate::decode::Picture::Cpu(frame) => gfx.renderer.upload(&frame),
                    #[cfg(zero_copy)]
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
            // How evenly frames are shown compared with how they were
            // captured (RFC 3550's jitter, on the screen).
            if let Some((shown, captured)) = self.last_shown {
                let d = (now.duration_since(shown).as_secs_f32() * 1000.0
                    - (ready.header.capture_us as f32 - captured as f32) / 1000.0)
                    .abs();
                self.shown_jitter_ms += (d - self.shown_jitter_ms) / 16.0;
            }
            self.last_shown = Some((now, ready.header.capture_us));
            self.first_frame.get_or_insert(now);
            self.frames_presented += 1;
            let _ = ready.decoded;
        }
        if self.occluded {
            return;
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
        #[allow(unused_mut)]
        let mut raw_input = gfx.egui.take_egui_input(&gfx.window);
        #[cfg(target_os = "android")]
        {
            raw_input.safe_area_insets =
                Some(crate::android::safe_area(gfx.window.scale_factor() as f32));
            // A text field in the menu takes the keyboard from the host.
            if self.egui_ctx.text_edit_focused() {
                crate::android::feed_egui(&self.egui_ctx, &mut raw_input);
            }
        }
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
        let quick = egui::Popup::is_any_open(&ctx);
        if quick != self.quick_menu_open {
            self.quick_menu_open = quick;
            self.release_all();
            self.update_cursor();
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
        #[cfg(target_os = "android")]
        let text = {
            let _ = gfx;
            crate::android::clipboard_text()
        };
        #[cfg(not(target_os = "android"))]
        let text = gfx.clipboard.get();
        let Some(text) = text else {
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
            Action::ToggleKeyboard => {
                #[cfg(target_os = "android")]
                crate::android::show_keyboard(!crate::android::keyboard_shown());
            }
            Action::KeyTap(usage) => self.tap_key(usage),
            Action::ToggleSticky(i) => {
                self.sticky[i] = !self.sticky[i];
                self.request_redraw();
            }
            Action::ResetZoom => {
                self.view = Default::default();
                self.request_redraw();
            }
            Action::DismissWarning(issue) => self.diagnostics.dismiss(issue),
            Action::Run(command) => self.run_command(command),
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
    DismissWarning(crate::diagnostics::Issue),
    /// A command from the quick menu.
    Run(Command),
    CloseMenu,
    SecureAttention,
    SelectDisplay(u32),
    MoveMenuButton([f32; 2]),
    MenuMoved([f32; 2]),
    /// The settings panel changed the config.
    SettingsChanged,
    Disconnect,
    /// Touch screens: show or hide the soft keyboard.
    ToggleKeyboard,
    /// Press and release a key (with the latched modifiers).
    KeyTap(u16),
    /// Latch a modifier (index into `STICKY`) for the next key.
    ToggleSticky(usize),
    ResetZoom,
}

/// Modifiers the touch key row latches: label and HID usage.
const STICKY: [(&str, u16); 4] = [
    ("Ctrl", 0xE0),
    ("Alt", 0xE2),
    ("Shift", 0xE1),
    ("Win", 0xE3),
];

/// Everything the overlay draws, copied out so the UI closure does not
/// borrow the viewer.
struct OverlayState {
    host_name: String,
    shortcut: String,
    menu_open: bool,
    show_stats: bool,
    fullscreen: bool,
    shortcuts: Bindings,
    /// The host's displays, and the one streamed (none for an extra display).
    displays: Vec<(u32, String)>,
    display: Option<u32>,
    stats_line: String,
    notices: Vec<String>,
    /// Connection warnings and what to say: (issue, text).
    warnings: Vec<(crate::diagnostics::Issue, String)>,
    /// One line each, sized to fit.
    short_warnings: bool,
    closed: Option<String>,
    remote_cursor: Option<(egui::TextureId, egui::Vec2, egui::Pos2)>,
    menu_button: String,
    menu_button_pos: Option<[f32; 2]>,
    menu_button_scale: f32,
    /// A touch screen (Android): touch controls and no shortcuts.
    touch: bool,
    keyboard: bool,
    sticky: [bool; 4],
    zoomed: bool,
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
            "{} {}×{} · {:.0} fps · {:.1} Mbit/s{} · encode {:.1} ms · decode {:.1} ms · capture→draw {:.1} ms · RTT {:.1} ms · jitter {:.1} ms{} · shown ±{:.1} ms\n{} · {}",
            s.codec,
            s.resolution.0,
            s.resolution.1,
            summary.fps,
            summary.mbps,
            v.host_health
                .filter(|(h, _)| h.bitrate_kbps > 0)
                .map(|(h, _)| format!(" (target {:.1})", h.bitrate_kbps as f32 / 1000.0))
                .unwrap_or_default(),
            summary.encode_ms,
            summary.decode_ms,
            v.latency_ms,
            v.session.rtt().as_secs_f32() * 1000.0,
            s.jitter_ms,
            v.buffer_ms
                .map(|b| format!(" · buffer {b:.0} ms"))
                .unwrap_or_default(),
            v.shown_jitter_ms,
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
                // On a phone the picture is scaled down a lot; keep the
                // cursor findable.
                let cs = if cfg!(target_os = "android") {
                    sx.max(ppp * 0.75)
                } else {
                    sx
                };
                let pos = egui::pos2(
                    (vp.x + c.x as f32 * sx - shape.hotspot_x as f32 * cs) / ppp,
                    (vp.y + c.y as f32 * sx - shape.hotspot_y as f32 * cs) / ppp,
                );
                Some((
                    tex.id(),
                    egui::vec2(shape.width as f32, shape.height as f32) * cs / ppp,
                    pos,
                ))
            })
            .flatten();
        OverlayState {
            host_name: v.host_name.clone(),
            shortcut: v.shortcuts.menu().to_string(),
            menu_open: v.menu_open,
            show_stats: v.in_force.show_stats,
            fullscreen: v.in_force.fullscreen,
            shortcuts: v.shortcuts.clone(),
            displays: v.displays.iter().map(|d| (d.id, d.name.clone())).collect(),
            display: if v.in_force.virtual_display.is_some() {
                None
            } else {
                v.display
                    .or_else(|| v.displays.iter().find(|d| d.primary).map(|d| d.id))
            },
            stats_line,
            notices: v
                .notices
                .iter()
                .filter(|(_, at)| at.elapsed() < Duration::from_secs(5))
                .map(|(t, _)| t.clone())
                .collect(),
            warnings: match v.in_force.warnings.as_str() {
                "off" => Vec::new(),
                style => v
                    .diagnostics
                    .warnings()
                    .into_iter()
                    .map(|f| (f.issue, if style == "short" { f.short } else { f.detail }))
                    .collect(),
            },
            short_warnings: v.in_force.warnings == "short",
            closed: v.closed.clone(),
            remote_cursor,
            menu_button: v.in_force.menu_button.clone(),
            menu_button_pos: v.config.menu_button_pos,
            menu_button_scale: v.config.menu_button_scale.clamp(0.5, 2.5),
            touch: cfg!(target_os = "android"),
            #[cfg(target_os = "android")]
            keyboard: crate::android::keyboard_shown(),
            #[cfg(not(target_os = "android"))]
            keyboard: false,
            sticky: v.sticky,
            zoomed: v.view.zoomed(),
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

    let screen = ctx.content_rect();
    if s.closed.is_none() {
        // Each warning and notice in its own area, centered below the last.
        let width = (screen.width() - 32.0).clamp(200.0, 520.0);
        let mut y = 12.0;
        for (n, (issue, text)) in s.warnings.iter().enumerate() {
            let shown = egui::Area::new(egui::Id::unique("warning").with(n))
                .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, y))
                .show(ctx, |ui| {
                    egui::Frame::popup(ui.style())
                        .fill(egui::Color32::from_rgba_unmultiplied(34, 39, 56, 235))
                        .stroke(egui::Stroke::new(1.0, theme::AMBER.gamma_multiply(0.6)))
                        .show(ui, |ui| {
                            if !s.short_warnings {
                                ui.set_width(width);
                            }
                            let row = |ui: &mut egui::Ui| {
                                // A warning sign: an amber disc with "!".
                                let (icon, _) = ui.allocate_exact_size(
                                    egui::vec2(18.0, 18.0),
                                    egui::Sense::hover(),
                                );
                                ui.painter().circle_filled(icon.center(), 8.0, theme::AMBER);
                                ui.painter().text(
                                    icon.center(),
                                    egui::Align2::CENTER_CENTER,
                                    "!",
                                    egui::FontId::proportional(13.0),
                                    egui::Color32::from_rgb(34, 39, 56),
                                );
                                if s.short_warnings {
                                    ui.add(egui::Label::new(text.as_str()).extend());
                                } else {
                                    let text_width = ui.available_width() - 34.0;
                                    ui.allocate_ui_with_layout(
                                        egui::vec2(text_width, 0.0),
                                        egui::Layout::top_down(egui::Align::Min),
                                        |ui| {
                                            ui.set_width(text_width);
                                            ui.add(egui::Label::new(text.as_str()).wrap());
                                        },
                                    );
                                }
                                if theme::close_button(ui)
                                    .on_hover_text("Hide until it happens again")
                                    .clicked()
                                {
                                    actions.push(Action::DismissWarning(*issue));
                                }
                            };
                            if s.short_warnings {
                                ui.horizontal(row);
                            } else {
                                ui.horizontal_top(row);
                            }
                        });
                });
            y += shown.response.rect.height() + 6.0;
        }
        for (n, text) in s.notices.iter().enumerate() {
            let shown = egui::Area::new(egui::Id::unique("notice").with(n))
                .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, y))
                .interactable(false)
                .show(ctx, |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.label(text);
                    });
                });
            y += shown.response.rect.height() + 6.0;
        }
    }

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

    // The always-available way in and out: a button with the shortcut
    // that can be dragged anywhere (or hidden, leaving the shortcut).
    if s.menu_button != "hidden" {
        menu_button(ctx, s, &mut actions);
    }
    if s.touch && s.closed.is_none() {
        touch_controls(ctx, s, &mut actions);
    }
    if !s.menu_open {
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
            Some(&format!(
                "Connected · {} shows or hides this menu",
                if s.touch { "Back" } else { &s.shortcut }
            )),
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
                crate::settings_ui::LiveAction::Hub(_) => {}
            }
        }
        if outcome.changed {
            actions.push(Action::SettingsChanged);
        }
    });
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

/// Clicks and drags, but no keyboard focus: egui gets the keys typed for
/// the host too, and Tab would focus the button, and Space then open the
/// menu.
const MENU_BUTTON_SENSE: egui::Sense = egui::Sense::CLICK.union(egui::Sense::DRAG);

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
        // No keyboard shortcuts on a phone.
        let text = match s.menu_button.as_str() {
            "logo" => String::new(),
            _ if s.touch => "Menu".to_string(),
            "shortcut" => s.shortcut.clone(),
            _ => format!("Menu  ·  {}", s.shortcut),
        };
        let logo = matches!(s.menu_button.as_str(), "full" | "logo");
        let galley = ui.painter().layout_no_wrap(
            text.clone(),
            egui::FontId::proportional(13.0 * s.menu_button_scale),
            egui::Color32::WHITE,
        );
        let k = s.menu_button_scale;
        let (h, pad, logo_size) = (30.0 * k, 12.0 * k, 18.0 * k);
        let gap = if logo && !text.is_empty() {
            8.0 * k
        } else {
            0.0
        };
        let w = if text.is_empty() {
            h
        } else {
            pad * 2.0 + if logo { logo_size } else { 0.0 } + gap + galley.size().x
        };
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(w, h), MENU_BUTTON_SENSE);
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
        let response = if s.menu_open {
            response.on_hover_text("Close the menu · drag to move")
        } else {
            response.on_hover_text("Quick actions · drag to move")
        };
        if response.dragged() {
            let half = egui::vec2(w / 2.0, h / 2.0);
            let c =
                (rect.center() + response.drag_delta()).clamp(screen.min + half, screen.max - half);
            let fx = (c.x - screen.min.x) / screen.width().max(1.0);
            let fy = (c.y - screen.min.y) / screen.height().max(1.0);
            actions.push(Action::MoveMenuButton([fx, fy]));
        } else if response.clicked() && s.menu_open {
            actions.push(Action::CloseMenu);
        }
        if !s.menu_open {
            let popup = egui::Popup::menu(&response);
            // Debugging aid for screenshots.
            let popup = if std::env::var_os("ORIEL_DEBUG_QUICK_MENU").is_some() {
                popup.open(true)
            } else {
                popup
            };
            popup.show(|ui| quick_menu(ui, s, actions));
        }
    });
}

/// The menu button's quick actions; the full menu is one of them.
fn quick_menu(ui: &mut egui::Ui, s: &OverlayState, actions: &mut Vec<Action>) {
    ui.set_min_width(220.0);
    let item = |ui: &mut egui::Ui, label: &str, command: Option<Command>, mark: Mark| {
        // Room on the left for the check or radio mark (painted: the font
        // has no check glyph).
        let mut button = egui::Button::new(format!("      {label}"));
        if let Some(key) = command.and_then(|c| s.shortcuts.get(c)) {
            button = button.shortcut_text(key.to_string());
        }
        let response = ui.add(button);
        paint_mark(ui, &response, mark);
        response.clicked()
    };
    if s.touch {
        if item(ui, "Keyboard", None, Mark::Check(s.keyboard)) {
            actions.push(Action::ToggleKeyboard);
            ui.close();
        }
        if s.zoomed && item(ui, "Reset zoom", None, Mark::None) {
            actions.push(Action::ResetZoom);
            ui.close();
        }
        ui.separator();
    }
    if item(ui, "Settings…", Some(Command::Menu), Mark::None) {
        actions.push(Action::OpenMenu);
        ui.close();
    }
    ui.separator();
    if s.displays.len() > 1 {
        for (id, name) in &s.displays {
            if item(ui, name, None, Mark::Radio(s.display == Some(*id))) {
                actions.push(Action::SelectDisplay(*id));
                ui.close();
            }
        }
        ui.separator();
    }
    if item(
        ui,
        "Full screen",
        Some(Command::Fullscreen),
        Mark::Check(s.fullscreen),
    ) {
        actions.push(Action::Run(Command::Fullscreen));
        ui.close();
    }
    if item(
        ui,
        "Statistics overlay",
        Some(Command::Stats),
        Mark::Check(s.show_stats),
    ) {
        actions.push(Action::Run(Command::Stats));
        ui.close();
    }
    if item(ui, "Send Ctrl+Alt+Del", None, Mark::None) {
        actions.push(Action::SecureAttention);
        ui.close();
    }
    ui.separator();
    let disconnect = egui::RichText::new("      Disconnect").color(crate::theme::RED);
    let mut button = egui::Button::new(disconnect);
    if let Some(key) = s.shortcuts.get(Command::Disconnect) {
        button = button.shortcut_text(key.to_string());
    }
    if ui.add(button).clicked() {
        actions.push(Action::Disconnect);
        ui.close();
    }
}

/// What a quick-menu item shows on its left.
#[derive(Clone, Copy)]
enum Mark {
    None,
    /// An on/off option.
    Check(bool),
    /// One of several (the displays).
    Radio(bool),
}

fn paint_mark(ui: &egui::Ui, response: &egui::Response, mark: Mark) {
    let (on, round) = match mark {
        Mark::None => return,
        Mark::Check(on) => (on, false),
        Mark::Radio(on) => (on, true),
    };
    let visuals = ui.style().interact(response);
    let size = 13.0;
    let center = egui::pos2(
        response.rect.left() + ui.spacing().button_padding.x + size / 2.0,
        response.rect.center().y,
    );
    let rect = egui::Rect::from_center_size(center, egui::vec2(size, size));
    let painter = ui.painter();
    let accent = ui.visuals().selection.bg_fill;
    let stroke = egui::Stroke::new(1.0, visuals.fg_stroke.color.gamma_multiply(0.7));
    if round {
        painter.circle_stroke(center, size / 2.0, stroke);
        if on {
            painter.circle_filled(center, size / 2.0 - 3.0, accent);
        }
    } else if on {
        painter.rect_filled(rect, 3.0, accent);
        let tick = [
            egui::pos2(rect.left() + 3.0, center.y),
            egui::pos2(center.x - 1.0, rect.bottom() - 3.5),
            egui::pos2(rect.right() - 3.0, rect.top() + 3.5),
        ];
        painter.line(tick.to_vec(), egui::Stroke::new(1.8, egui::Color32::WHITE));
    } else {
        painter.rect_stroke(rect, 3.0, stroke, egui::StrokeKind::Inside);
    }
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
        // Android: "full screen" hides the status and navigation bars.
        #[cfg(target_os = "android")]
        crate::android::set_immersive(self.in_force.fullscreen);
        let renderer = match Renderer::new(window.clone()) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("cannot initialise rendering: {e:#}");
                event_loop.exit();
                return;
            }
        };
        // A fresh context: textures egui already sent belonged to the
        // previous renderer (Android recreates it after the background).
        if self.gfx.is_none() && !self.egui_cursors.is_empty() {
            self.egui_cursors.clear();
        }
        self.egui_ctx = egui::Context::default();
        crate::theme::style(&self.egui_ctx);
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
        // The decoder thread uses it only while hardware decoding is on
        // (PyroWave decodes on the GPU either way).
        #[cfg(zero_copy)]
        {
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
        // Android said "focused" once, to the launcher: a shown app has it.
        if cfg!(target_os = "android") {
            self.set_focused(true);
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        let Some(gfx) = &mut self.gfx else { return };
        // The launcher's window stays open behind this one: its events
        // (close, resize, visibility) are not the viewer's.
        if id != gfx.window.id() {
            return;
        }
        let response = gfx.egui.on_window_event(&gfx.window, &event);
        // egui-winit asks for a repaint on nearly every event (including
        // redraws themselves). Video frames drive redraws; the overlay only
        // needs extra ones while the menu is open or the pointer is on it.
        let overlay_active =
            self.menu_open || self.quick_menu_open || self.pointer_over_ui || self.closed.is_some();
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
            WindowEvent::Occluded(occluded) => {
                tracing::info!(occluded, "window visibility changed");
                self.occluded = occluded;
                if !occluded {
                    gfx.window.request_redraw();
                }
            }
            WindowEvent::Focused(focused) => self.set_focused(focused),
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            WindowEvent::KeyboardInput { event, .. } => self.handle_key(&event),
            WindowEvent::CursorMoved { position, .. } => self.handle_pointer_moved(position),
            WindowEvent::Touch(t) => self.handle_touch(t),
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
            // A buffered frame is drawn when due (see `about_to_wait`).
            UiEvent::FrameReady => {
                self.frame_pending
                    .store(false, std::sync::atomic::Ordering::Release);
                if self
                    .session
                    .frames
                    .next_due()
                    .is_none_or(|due| due <= Instant::now())
                {
                    self.request_redraw();
                }
            }
            UiEvent::Platform => {
                #[cfg(target_os = "android")]
                self.handle_platform();
                self.request_redraw();
            }
            UiEvent::HookKeys => {
                #[cfg(windows)]
                self.handle_hook_keys();
            }
            UiEvent::Refresh => {}
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
        self.rescale_cursors(event_loop);
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
        self.diagnose();
        // Wake for the clipboard while focused; redraw only for overlays.
        // Diagnostics need a look once a second either way.
        let periodic = self.overlay_ticks() || (self.focused && self.closed.is_none());
        let mut wake = if periodic {
            Some(Instant::now() + Duration::from_millis(250))
        } else if self.closed.is_none() {
            Some(self.diagnosed + Duration::from_secs(1))
        } else {
            None
        };
        // The next buffered frame.
        if let Some(due) = self.session.frames.next_due() {
            wake = Some(wake.map_or(due, |w| w.min(due)));
        }
        // Keep confirming held keys, or the host stops repeating them.
        if !self.keys_down.is_empty() {
            if self.keys_held_sent.elapsed() >= oriel_proto::KEYS_HELD_INTERVAL {
                self.session.input(InputEvent::KeysHeld {
                    usages: self.keys_down.iter().copied().collect(),
                });
                self.keys_held_sent = Instant::now();
            }
            let due = self.keys_held_sent + oriel_proto::KEYS_HELD_INTERVAL;
            wake = Some(wake.map_or(due, |w| w.min(due)));
        }
        // A finger held still becomes a right click.
        let actions = self.gestures.tick(Instant::now());
        self.apply_touch(actions);
        if let Some(due) = self.gestures.deadline() {
            wake = Some(wake.map_or(due, |w| w.min(due)));
        }
        event_loop.set_control_flow(wake.map_or(ControlFlow::Wait, ControlFlow::WaitUntil));
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        #[cfg(target_os = "android")]
        crate::android::set_immersive(false);
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

    fn suspended(&mut self, _event_loop: &ActiveEventLoop) {
        // Android takes the surface away while in the background; the
        // session keeps running and the window comes back on resume.
        if cfg!(target_os = "android") {
            self.set_focused(false);
            self.egui_cursors.clear();
            self.gfx = None;
        }
    }

    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: StartCause) {
        // Android announces its one window only once, before the launcher.
        #[cfg(target_os = "android")]
        if matches!(cause, StartCause::Init) && crate::android::app().native_window().is_some() {
            self.resumed(event_loop);
        }
        let _ = event_loop;
        if !matches!(cause, StartCause::ResumeTimeReached { .. }) {
            return;
        }
        if self
            .session
            .frames
            .next_due()
            .is_some_and(|due| due <= Instant::now())
        {
            self.request_redraw();
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

/// The key that types `c` on a US layout (letters, digits, space).
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
fn ascii_usage(c: char) -> Option<u16> {
    match c.to_ascii_lowercase() {
        c @ 'a'..='z' => Some(0x04 + (c as u16 - 'a' as u16)),
        c @ '1'..='9' => Some(0x1E + (c as u16 - '1' as u16)),
        '0' => Some(0x27),
        ' ' => Some(0x2C),
        _ => None,
    }
}

/// Touch screens: a keyboard button, and while the keyboard is up, the
/// keys phones lack (Esc, Tab, modifiers that latch, arrows).
fn touch_controls(ctx: &egui::Context, s: &OverlayState, actions: &mut Vec<Action>) {
    let fill = egui::Color32::from_rgba_unmultiplied(34, 39, 56, 210);
    egui::Area::new(egui::Id::unique("keyboard-button"))
        .order(egui::Order::Foreground)
        .anchor(egui::Align2::LEFT_BOTTOM, egui::vec2(8.0, -8.0))
        .show(ctx, |ui| {
            let size = egui::vec2(44.0, 30.0) * s.menu_button_scale;
            let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
            let p = ui.painter();
            p.rect_filled(rect, size.y / 2.0, fill);
            // A keyboard: an outline with rows of keys.
            let k = rect.shrink2(egui::vec2(size.x * 0.25, size.y * 0.27));
            let stroke = egui::Stroke::new(1.3, egui::Color32::WHITE);
            p.rect_stroke(k, 2.0, stroke, egui::StrokeKind::Middle);
            for row in 0..2 {
                for col in 0..4 {
                    let c = egui::pos2(
                        k.left() + k.width() * (col as f32 + 0.75) / 4.5,
                        k.top() + k.height() * (row as f32 + 0.8) / 3.0,
                    );
                    p.circle_filled(c, 1.0, egui::Color32::WHITE);
                }
            }
            p.line_segment(
                [
                    egui::pos2(k.left() + k.width() * 0.3, k.bottom() - k.height() * 0.2),
                    egui::pos2(k.right() - k.width() * 0.3, k.bottom() - k.height() * 0.2),
                ],
                stroke,
            );
            if response
                .on_hover_text(if s.keyboard {
                    "Hide the keyboard"
                } else {
                    "Keyboard"
                })
                .clicked()
            {
                actions.push(Action::ToggleKeyboard);
            }
        });
    if !s.keyboard {
        return;
    }
    egui::Area::new(egui::Id::unique("key-row"))
        .order(egui::Order::Foreground)
        .anchor(
            egui::Align2::CENTER_BOTTOM,
            egui::vec2(0.0, -46.0 * s.menu_button_scale),
        )
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).fill(fill).show(ui, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
                // Arrows on a second row on a narrow (portrait) phone.
                let narrow = ctx.content_rect().width() < 520.0;
                ui.horizontal_wrapped(|ui| {
                    ui.set_max_width(if narrow { 300.0 } else { 600.0 });
                    let key = |ui: &mut egui::Ui, label: &str, on: bool| {
                        ui.add(
                            egui::Button::new(egui::RichText::new(label).size(13.0))
                                .selected(on)
                                .min_size(egui::vec2(30.0, 30.0)),
                        )
                        .clicked()
                    };
                    if key(ui, "Esc", false) {
                        actions.push(Action::KeyTap(0x29));
                    }
                    if key(ui, "Tab", false) {
                        actions.push(Action::KeyTap(0x2B));
                    }
                    for (i, (label, _)) in STICKY.iter().enumerate() {
                        if key(ui, label, s.sticky[i]) {
                            actions.push(Action::ToggleSticky(i));
                        }
                    }
                    // Arrows, painted (the font has none): left, up, down, right.
                    for (usage, dir) in [(0x50u16, 0), (0x52, 1), (0x51, 2), (0x4F, 3)] {
                        let r = ui.add(egui::Button::new("").min_size(egui::vec2(30.0, 30.0)));
                        let c = r.rect.center();
                        let (a, b, t) = match dir {
                            0 => ((4.0, -5.0), (4.0, 5.0), (-4.0, 0.0)),
                            1 => ((-5.0, 4.0), (5.0, 4.0), (0.0, -4.0)),
                            2 => ((-5.0, -4.0), (5.0, -4.0), (0.0, 4.0)),
                            _ => ((-4.0, -5.0), (-4.0, 5.0), (4.0, 0.0)),
                        };
                        let at = |(x, y): (f32, f32)| c + egui::vec2(x, y);
                        ui.painter().add(egui::Shape::convex_polygon(
                            vec![at(a), at(b), at(t)],
                            egui::Color32::WHITE,
                            egui::Stroke::NONE,
                        ));
                        if r.clicked() {
                            actions.push(Action::KeyTap(usage));
                        }
                    }
                });
            });
        });
}

/// Android's Back button (winit calls it BrowserBack).
pub(crate) fn is_back(event: &KeyEvent) -> bool {
    use winit::keyboard::{Key, NamedKey};
    cfg!(target_os = "android")
        && matches!(
            event.logical_key,
            Key::Named(NamedKey::BrowserBack | NamedKey::GoBack)
        )
}

/// The codecs to ask for at 4:4:4: those FFmpeg decodes here (in hardware
/// where it can, else in software). Not on Android, whose decoders rarely
/// can, nor for PyroWave (4:2:0 only for now).
fn full_chroma_codecs(wanted: bool) -> Vec<oriel_proto::Codec> {
    if wanted && !cfg!(target_os = "android") {
        vec![oriel_proto::Codec::Hevc, oriel_proto::Codec::H264]
    } else {
        Vec::new()
    }
}

/// The codecs to ask the host for: the choice, minus PyroWave where this
/// client cannot decode it (macOS, Android, or the library is missing).
fn offered_codecs(choice: &str) -> (Vec<oriel_proto::Codec>, bool) {
    let (mut codecs, auto) = crate::config::codec_choice(choice);
    if codecs.contains(&oriel_proto::Codec::PyroWave) && !oriel_media::pyrowave_available() {
        tracing::info!("PyroWave cannot be decoded here; not offering it");
        codecs.retain(|c| *c != oriel_proto::Codec::PyroWave);
        return (codecs, false);
    }
    (codecs, auto)
}

#[cfg(test)]
mod tests {
    /// Clicks on a button with `sense` after `keys` were typed (each
    /// pressed and released in its own pass).
    fn clicks_after(sense: egui::Sense, keys: &[egui::Key]) -> bool {
        let ctx = egui::Context::default();
        let mut clicked = false;
        let mut pass = |events: Vec<egui::Event>| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 600.0),
                )),
                events,
                ..Default::default()
            };
            let mut output = ctx.run_ui(input, |ui| {
                egui::Area::new(egui::Id::unique("button")).show(ui.ctx(), |ui| {
                    let (_, response) = ui.allocate_exact_size(egui::vec2(30.0, 30.0), sense);
                    clicked |= response.clicked();
                });
            });
            output.textures_delta.clear();
        };
        pass(vec![]);
        for &key in keys {
            for pressed in [true, false] {
                pass(vec![egui::Event::Key {
                    key,
                    physical_key: Some(key),
                    pressed,
                    repeat: false,
                    modifiers: Default::default(),
                }]);
            }
        }
        clicked
    }

    #[test]
    fn typing_does_not_press_the_menu_button() {
        use egui::Key::{Enter, Space, Tab};
        assert!(clicks_after(egui::Sense::click_and_drag(), &[Tab, Space]));
        assert!(!clicks_after(super::MENU_BUTTON_SENSE, &[Tab, Space]));
        assert!(!clicks_after(super::MENU_BUTTON_SENSE, &[Tab, Tab, Enter]));
    }
}
