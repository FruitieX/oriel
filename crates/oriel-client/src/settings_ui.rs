//! The settings panel, shared by the launcher's Settings (the defaults),
//! a host's settings, and the session menu (that host's settings, applied
//! live). Every setting has a default and an optional per-host override.

use egui::Ui;
use oriel_proto::{Codec, VirtualMode};

use crate::{
    config::{
        CAPTURE_CHOICES, CODEC_CHOICES, CODER_CHOICES, ClientConfig, HostPrefs, MENU_BUTTON_STYLES,
        MOUSE_MODES, Resolution, SMOOTHING_CHOICES, WARNING_STYLES, codec_choice_strings,
        format_resolution, format_virtual_mode,
    },
    keymap::{Command, Shortcut},
    theme::{self, AMBER},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Display,
    Video,
    Input,
    Sound,
    Interface,
    App,
}

/// What the connected host offers (the session menu only).
pub struct Live {
    pub displays: Vec<(u32, String)>,
    /// The physical display being streamed.
    pub display: Option<u32>,
    pub virtual_ok: bool,
    /// The mode of the client's own screen.
    pub monitor_mode: Option<VirtualMode>,
    /// Resolutions the streamed display can switch to.
    pub modes: Vec<(u32, u32)>,
    pub host_codecs: Vec<Codec>,
    /// Host sound outputs as (id, name).
    pub audio_devices: Vec<(String, String)>,
}

/// Choices that are not settings: they act on the running session.
pub enum LiveAction {
    SelectDisplay(u32),
    /// The hub account (start window only).
    Hub(crate::hub::HubAction),
}

#[derive(Default)]
pub struct Outcome {
    /// The config was changed (save it, apply it).
    pub changed: bool,
    pub actions: Vec<LiveAction>,
}

pub struct Panel {
    pub tab: Tab,
    /// The command whose shortcut the next key press sets.
    recording: Option<Command>,
    hint: String,
    /// Local devices, listed when the Sound tab is first shown.
    devices: Option<(
        Vec<oriel_audio::OutputDevice>,
        Vec<oriel_audio::OutputDevice>,
    )>,
    /// The hub account section of the App tab (start window only).
    pub hub: Option<crate::hub::HubView>,
}

impl Panel {
    pub fn new(host: bool) -> Self {
        Self {
            tab: if host { Tab::Display } else { Tab::Video },
            recording: None,
            hint: String::new(),
            devices: None,
            hub: None,
        }
    }

    /// Waiting for the keys of a new shortcut.
    pub fn is_recording(&self) -> bool {
        self.recording.is_some()
    }

    /// Forget listed devices (they are listed again when shown).
    pub fn refresh_devices(&mut self) {
        self.devices = None;
    }

    pub fn tabs(host: bool) -> Vec<(Tab, &'static str)> {
        let mut tabs = Vec::new();
        if host {
            tabs.push((Tab::Display, "Display"));
        }
        tabs.extend([
            (Tab::Video, "Video"),
            (Tab::Input, "Input"),
            (Tab::Sound, "Sound"),
            (Tab::Interface, "Interface"),
        ]);
        if !host {
            tabs.push((Tab::App, "App"));
        }
        tabs
    }

    /// The tab bar and the current tab in a fixed `size`, scrolling what
    /// does not fit. Edits go to `host`'s overrides, or to the defaults.
    pub fn show(
        &mut self,
        ui: &mut Ui,
        config: &mut ClientConfig,
        host: Option<&str>,
        live: Option<&Live>,
        size: egui::Vec2,
    ) -> Outcome {
        if let Some(command) = self.recording {
            self.record_shortcut(ui, config, command);
        }
        let tabs = Self::tabs(host.is_some());
        if !tabs.iter().any(|(t, _)| *t == self.tab) {
            self.tab = tabs[0].0;
        }
        theme::tab_bar(ui, &mut self.tab, &tabs);
        ui.add_space(6.0);
        let mut rows = Rows {
            config,
            host,
            field: if size.x < COMPACT_WIDTH {
                size.x.min(300.0)
            } else {
                (size.x - 200.0).clamp(150.0, 300.0)
            },
            outcome: Outcome::default(),
        };
        egui::ScrollArea::vertical()
            .id_salt("settings-panel")
            .max_height(size.y)
            .min_scrolled_height(size.y)
            .auto_shrink(false)
            .show(ui, |ui| {
                ui.set_width(size.x);
                match self.tab {
                    Tab::Display => display_tab(ui, &mut rows, live),
                    Tab::Video => video_tab(ui, &mut rows, live),
                    Tab::Input => self.input_tab(ui, &mut rows),
                    Tab::Sound => self.sound_tab(ui, &mut rows, live),
                    Tab::Interface => interface_tab(ui, &mut rows),
                    Tab::App => app_tab(ui, &mut rows, self.hub.as_mut()),
                }
            });
        rows.outcome
    }

    fn input_tab(&mut self, ui: &mut Ui, rows: &mut Rows) {
        grid(ui, "input", |ui| {
            rows.choice(
                ui,
                "Mouse",
                "Desktop: the pointer moves freely. Captured: relative motion for games. \
                 Automatic switches when a game captures the pointer.",
                |c| c.mouse.clone(),
                |c, v| c.mouse = v,
                |h| &mut h.mouse,
                &MOUSE_MODES,
            );
            rows.toggle(
                ui,
                "Gamepads",
                "Forward controllers",
                "Controllers appear on the host as Xbox pads while this window has focus",
                |c| c.gamepads,
                |c, v| c.gamepads = v,
                |h| &mut h.gamepads,
            );
        });
        ui.add_space(14.0);
        theme::section_title(ui, "Shortcuts");
        if rows.host.is_some() {
            theme::note(ui, "Shortcuts are the same for every computer.");
            ui.add_space(4.0);
        }
        let mut bindings = rows.config.bindings();
        let before = bindings.clone();
        grid(ui, "shortcuts", |ui| {
            for command in Command::ALL {
                ui.label(command.label());
                let recording = self.recording == Some(command);
                let current = bindings.get(command);
                let text = if recording {
                    "Press the new shortcut...".to_string()
                } else {
                    current.map_or_else(|| "None".into(), |s| s.to_string())
                };
                let response = ui
                    .add(
                        egui::Button::new(text)
                            .min_size(egui::vec2(rows.field.min(220.0), 0.0))
                            .selected(recording),
                    )
                    .on_hover_text("Click, then press the keys (Esc cancels)");
                if response.clicked() {
                    // Keep Space/Enter from clicking it again.
                    response.surrender_focus();
                    self.recording = (!recording).then_some(command);
                    self.hint.clear();
                }
                ui.horizontal(|ui| {
                    if command != Command::Menu
                        && current.is_some()
                        && ui.small_button("Clear").clicked()
                    {
                        bindings.set(command, None);
                    }
                    let default = Shortcut::parse(command.default_shortcut());
                    if current != default && ui.small_button("Reset").clicked() {
                        bindings.set(command, default);
                    }
                });
                ui.end_row();
            }
        });
        if !self.hint.is_empty() {
            ui.colored_label(AMBER, &self.hint);
        }
        if bindings != before {
            rows.config.set_bindings(&bindings);
            rows.outcome.changed = true;
        }
    }

    /// Take the next key press as the shortcut for `command`.
    fn record_shortcut(&mut self, ui: &mut Ui, config: &mut ClientConfig, command: Command) {
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
                self.hint = "Include Ctrl, Alt or Shift, so the key still reaches the host.".into();
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
                self.hint = format!("{} cannot be used in a shortcut.", key.name());
                continue;
            };
            let mut bindings = config.bindings();
            // One shortcut, one command.
            for other in Command::ALL {
                if other != command && bindings.get(other) == Some(shortcut) {
                    if other == Command::Menu {
                        self.hint = format!("{shortcut} already opens the menu.");
                        return;
                    }
                    bindings.set(other, None);
                    self.hint = format!("Removed {shortcut} from {}.", other.label());
                }
            }
            bindings.set(command, Some(shortcut));
            config.set_bindings(&bindings);
            self.recording = None;
            return;
        }
    }

    fn sound_tab(&mut self, ui: &mut Ui, rows: &mut Rows, live: Option<&Live>) {
        let (outputs, mics) = self
            .devices
            .get_or_insert_with(|| (oriel_audio::output_devices(), oriel_audio::input_devices()));
        let outputs: Vec<(String, String)> = outputs
            .iter()
            .map(|d| (d.id.clone(), d.name.clone()))
            .collect();
        let mics: Vec<(String, String)> = mics
            .iter()
            .map(|d| (d.id.clone(), d.name.clone()))
            .collect();
        grid(ui, "sound", |ui| {
            rows.toggle(
                ui,
                "Sound",
                "Play the remote computer's sound",
                "",
                |c| c.audio,
                |c, v| c.audio = v,
                |h| &mut h.audio,
            );
            rows.row(
                ui,
                "Sound quality",
                "Opus bitrate of the remote sound; 128 kbit/s is already \
                 transparent for most music",
                |c| c.audio_kbps,
                |c, v| c.audio_kbps = v,
                |h| &mut h.audio_kbps,
                |ui, kbps, width| {
                    const CHOICES: [u32; 6] = [64, 96, 128, 192, 256, 320];
                    egui::ComboBox::from_id_salt("audio-kbps")
                        .width(width)
                        .selected_text(format!("{kbps} kbit/s"))
                        .show_ui(ui, |ui| {
                            for choice in CHOICES {
                                ui.selectable_value(kbps, choice, format!("{choice} kbit/s"));
                            }
                        });
                },
            );
            rows.toggle(
                ui,
                "Host speakers",
                "Mute while connected",
                "Silences the host's own speakers during the session and restores \
                 them afterwards (Windows hosts); the stream still has the sound",
                |c| c.mute_host,
                |c, v| c.mute_host = v,
                |h| &mut h.mute_host,
            );
            rows.device(
                ui,
                "Play on",
                "Default output",
                &outputs,
                |c| c.playback_device.clone(),
                |c, v| c.playback_device = v,
                |h| &mut h.playback_device,
            );
            if let Some(live) = live.filter(|l| !l.audio_devices.is_empty()) {
                ui.label("Sound from")
                    .on_hover_text("Which of the host's outputs you hear");
                let host = rows.host.unwrap_or_default().to_string();
                let current = rows.config.effective(Some(&host)).audio_device;
                let mut value = current.clone();
                let name = |id: &Option<String>| match id {
                    None => "Host's default output".to_string(),
                    Some(id) => live
                        .audio_devices
                        .iter()
                        .find(|(i, _)| i == id)
                        .map_or_else(|| id.clone(), |(_, n)| n.clone()),
                };
                egui::ComboBox::from_id_salt("host-audio")
                    .width(rows.field)
                    .selected_text(name(&value))
                    .show_ui(ui, |ui| {
                        for choice in std::iter::once(None)
                            .chain(live.audio_devices.iter().map(|(id, _)| Some(id.clone())))
                        {
                            let label = name(&choice);
                            ui.selectable_value(&mut value, choice, label);
                        }
                    });
                ui.label("");
                ui.end_row();
                if value != current {
                    rows.config.host_mut(&host).audio_device = value;
                    rows.outcome.changed = true;
                }
            }
            rows.toggle(
                ui,
                "Microphone",
                "Send my microphone",
                "Apps on the host can record it as a microphone",
                |c| c.microphone,
                |c, v| c.microphone = v,
                |h| &mut h.microphone,
            );
            rows.device(
                ui,
                "Microphone device",
                "Default microphone",
                &mics,
                |c| c.microphone_device.clone(),
                |c, v| c.microphone_device = v,
                |h| &mut h.microphone_device,
            );
        });
        if live.is_none() && rows.host.is_some() {
            ui.add_space(8.0);
            theme::note(
                ui,
                "Which of the host's outputs you hear is chosen while connected.",
            );
        }
    }
}

fn display_tab(ui: &mut Ui, rows: &mut Rows, live: Option<&Live>) {
    let host = rows.host.unwrap_or_default().to_string();
    if let Some(live) = live {
        let effective = rows.config.effective(Some(&host));
        theme::section_title(ui, "Stream");
        ui.horizontal_wrapped(|ui| {
            for (id, name) in &live.displays {
                let selected = effective.virtual_display.is_none() && live.display == Some(*id);
                if ui.selectable_label(selected, name).clicked() && !selected {
                    rows.outcome.actions.push(LiveAction::SelectDisplay(*id));
                    rows.config.host_mut(&host).virtual_display = None;
                    rows.outcome.changed = true;
                }
            }
            if live.virtual_ok {
                let on = effective.virtual_display.is_some();
                if ui
                    .selectable_label(on, "Extra display")
                    .on_hover_text("Extend the host's desktop with a new monitor shown here")
                    .clicked()
                    && !on
                {
                    let mode = live.monitor_mode.unwrap_or(VirtualMode {
                        width: 1920,
                        height: 1080,
                        refresh_hz: 60,
                    });
                    rows.config.host_mut(&host).virtual_display = Some(format_virtual_mode(&mode));
                    rows.outcome.changed = true;
                }
            }
        });
        ui.add_space(10.0);
        grid(ui, "display", |ui| {
            if let Some(mode) = effective.virtual_display {
                virtual_mode_rows(ui, rows, &host, live, mode, effective.resolution);
            } else if !live.modes.is_empty() {
                ui.label("Resolution")
                    .on_hover_text("Switches the host's display for the session");
                let label = |r: Resolution| match r {
                    Resolution::Fixed(w, h) => format!("{w}×{h}"),
                    Resolution::Client(_) => "This screen".to_string(),
                    Resolution::Host => "Host's own".to_string(),
                };
                let current = effective.resolution;
                let scale = match current {
                    Resolution::Client(s) => s,
                    _ => 1.0,
                };
                let mut value = current;
                egui::ComboBox::from_id_salt("resolution")
                    .width(rows.field)
                    .selected_text(label(value))
                    .show_ui(ui, |ui| {
                        let choices = [Resolution::Host, Resolution::Client(scale)]
                            .into_iter()
                            .chain(live.modes.iter().map(|&(w, h)| Resolution::Fixed(w, h)));
                        for r in choices {
                            ui.selectable_value(&mut value, r, label(r));
                        }
                    })
                    .response
                    .on_hover_text(
                        "This screen: the resolution of the screen oriel shows it on, \
                         scaled. Displays that cannot show it get the closest size they \
                         support.",
                    );
                ui.label("");
                ui.end_row();
                if let Resolution::Client(scale) = current
                    && let Some(v) = scale_row(ui, scale)
                {
                    value = Resolution::Client(v);
                }
                if value != current {
                    save_resolution(rows, &host, value);
                }
            }
            fullscreen_row(ui, rows);
        });
    } else {
        grid(ui, "display", |ui| fullscreen_row(ui, rows));
        ui.add_space(8.0);
        theme::note(
            ui,
            "The display to stream, its resolution and the extra display are chosen while \
             connected, and remembered for this computer.",
        );
    }
}

fn fullscreen_row(ui: &mut Ui, rows: &mut Rows) {
    rows.toggle(
        ui,
        "Fullscreen",
        "Fill the screen",
        "",
        |c| c.fullscreen,
        |c, v| c.fullscreen = v,
        |h| &mut h.fullscreen,
    );
}

fn virtual_mode_rows(
    ui: &mut Ui,
    rows: &mut Rows,
    host: &str,
    live: &Live,
    mode: VirtualMode,
    resolution: Resolution,
) {
    let mut sizes = vec![
        (1280, 720),
        (1600, 900),
        (1920, 1080),
        (2560, 1440),
        (3840, 2160),
    ];
    let mut rates = vec![30, 60, 75, 90, 120, 144, 165, 240];
    if let Some(m) = live.monitor_mode {
        sizes.push((m.width, m.height));
        rates.push(m.refresh_hz);
    }
    sizes.push((mode.width, mode.height));
    rates.push(mode.refresh_hz);
    sizes.sort_by_key(|(w, h)| w * h);
    sizes.dedup();
    rates.sort();
    rates.dedup();
    let native = |w, h| {
        live.monitor_mode
            .is_some_and(|m| (m.width, m.height) == (w, h))
    };
    let mut value = mode;
    // "This screen" sizes the extra display to the screen oriel shows it
    // on (scaled), following it when the window moves or the phone turns.
    let follows = match resolution {
        Resolution::Client(scale) => Some(scale),
        _ => None,
    };
    let mut follow = follows;
    ui.label("Size");
    egui::ComboBox::from_id_salt("virtual-size")
        .width(rows.field)
        .selected_text(match follows {
            Some(_) => "This screen".to_string(),
            None => format!("{}×{}", mode.width, mode.height),
        })
        .show_ui(ui, |ui| {
            if ui
                .selectable_label(follows.is_some(), "This screen")
                .clicked()
            {
                follow = Some(follows.unwrap_or(1.0));
            }
            for (w, h) in sizes {
                let label = if native(w, h) {
                    format!("{w}×{h} (this screen)")
                } else {
                    format!("{w}×{h}")
                };
                if ui
                    .selectable_label(
                        follows.is_none() && (value.width, value.height) == (w, h),
                        label,
                    )
                    .clicked()
                {
                    value.width = w;
                    value.height = h;
                    follow = None;
                }
            }
        });
    ui.label("");
    ui.end_row();
    if let Some(scale) = follows
        && let Some(v) = scale_row(ui, scale)
    {
        follow = Some(v);
    }
    ui.label("Refresh rate");
    egui::ComboBox::from_id_salt("virtual-rate")
        .width(rows.field)
        .selected_text(format!("{} Hz", mode.refresh_hz))
        .show_ui(ui, |ui| {
            for hz in rates {
                ui.selectable_value(&mut value.refresh_hz, hz, format!("{hz} Hz"));
            }
        });
    ui.label("");
    ui.end_row();
    if value != mode {
        rows.config.host_mut(host).virtual_display = Some(format_virtual_mode(&value));
        rows.outcome.changed = true;
    }
    if follow != follows {
        save_resolution(
            rows,
            host,
            follow.map_or(Resolution::Host, Resolution::Client),
        );
    }
}

fn video_tab(ui: &mut Ui, rows: &mut Rows, live: Option<&Live>) {
    let codecs: Vec<(&str, &str)> = CODEC_CHOICES
        .iter()
        .copied()
        .filter(|(key, _)| {
            let Some(live) = live else { return true };
            let has = |c| live.host_codecs.contains(&c);
            match *key {
                "auto" | "pyrowave" => has(Codec::PyroWave),
                "h264" => has(Codec::H264),
                "av1" => has(Codec::Av1),
                _ => has(Codec::Hevc),
            }
        })
        .collect();
    grid(ui, "video", |ui| {
        rows.choice(
            ui,
            "Codec",
            "Automatic uses PyroWave on fast, low-latency links and HEVC otherwise",
            |c| c.codec_key(),
            |c, v| c.codecs = codec_choice_strings(&v),
            |h| &mut h.codec,
            &codecs,
        );
        // One bitrate per codec family, remembered separately: PyroWave
        // needs far more. Automatic switches between them, so it shows both.
        let codec = rows.config.effective(rows.host).codec;
        if codec != "pyrowave" {
            rows.mbps(
                ui,
                if codec == "auto" {
                    "HEVC bitrate"
                } else {
                    "Bitrate"
                },
                "The most it uses when automatic bitrate is on",
                2..=500,
                |c| c.bitrate_kbps,
                |c, v| c.bitrate_kbps = v,
                |h| &mut h.bitrate_kbps,
            );
        }
        if codec == "pyrowave" || codec == "auto" {
            rows.mbps(
                ui,
                if codec == "auto" {
                    "PyroWave bitrate"
                } else {
                    "Bitrate"
                },
                "PyroWave needs far more bandwidth than the other codecs; this is \
                 remembered apart from theirs",
                50..=1500,
                |c| c.pyrowave_kbps,
                |c, v| c.pyrowave_kbps = v,
                |h| &mut h.pyrowave_kbps,
            );
        }
        // PyroWave keeps its bitrate and is 4:2:0 only.
        let pyrowave = codec == "pyrowave";
        if !pyrowave {
            rows.toggle(
                ui,
                "Automatic bitrate",
                "Follow the network",
                "Lowers the bitrate within a fraction of a second when the network \
             cannot carry it, and raises it again once it can",
                |c| c.auto_bitrate,
                |c, v| c.auto_bitrate = v,
                |h| &mut h.auto_bitrate,
            );
        }
        // Android decoders rarely handle 4:4:4.
        if !cfg!(target_os = "android") && !pyrowave {
            rows.toggle(
                ui,
                "Colour detail",
                "Full colour resolution (4:4:4)",
                "Keeps coloured text and thin coloured lines crisp. Needs a host \
                 that can encode it (NVIDIA, or software encoding) and takes more \
                 bandwidth; this computer may decode it in software",
                |c| c.full_chroma,
                |c, v| c.full_chroma = v,
                |h| &mut h.full_chroma,
            );
        }
        if !cfg!(target_os = "android") {
            rows.choice(
                ui,
                "Decoding",
                "Hardware falls back to software for what the GPU cannot decode",
                |c| c.decoding.clone(),
                |c, v| c.decoding = v,
                |h| &mut h.decoding,
                &CODER_CHOICES,
            );
        }
        rows.choice(
            ui,
            "Encoding on the host",
            "Software encoding uses the host's CPU (x264, x265, SVT-AV1) where it \
             has those; hardware falls back to software where the GPU cannot encode",
            |c| c.encoding.clone(),
            |c, v| c.encoding = v,
            |h| &mut h.encoding,
            &CODER_CHOICES,
        );
        rows.choice(
            ui,
            "Capture on the host",
            "Windows hosts. Graphics Capture keeps the cursor out of the picture \
             (Windows sometimes draws it in, e.g. with several monitors) but cannot \
             show the lock screen; Desktop Duplication is used where it fails",
            |c| c.capture.clone(),
            |c, v| c.capture = v,
            |h| &mut h.capture,
            &CAPTURE_CHOICES,
        );
        rows.choice(
            ui,
            "Smoothness",
            "Holds frames briefly so that uneven network delay (jitter) does not \
             make motion stutter; adds that much latency",
            |c| c.smoothing.clone(),
            |c, v| c.smoothing = v,
            |h| &mut h.smoothing,
            &SMOOTHING_CHOICES,
        );
        rows.row(
            ui,
            "Frame rate",
            "The most frames per second the host sends",
            |c| c.max_fps,
            |c, v| c.max_fps = v,
            |h| &mut h.max_fps,
            |ui, v, w| {
                egui::ComboBox::from_id_salt("fps")
                    .width(w)
                    .selected_text(format!("{v} fps"))
                    .show_ui(ui, |ui| {
                        for fps in [30, 60, 90, 120, 144, 165, 240] {
                            ui.selectable_value(v, fps, format!("{fps} fps"));
                        }
                    });
            },
        );
    });
}

fn interface_tab(ui: &mut Ui, rows: &mut Rows) {
    grid(ui, "interface", |ui| {
        rows.toggle(
            ui,
            "Statistics",
            "Show the statistics overlay",
            "",
            |c| c.show_stats,
            |c, v| c.show_stats = v,
            |h| &mut h.show_stats,
        );
        rows.choice(
            ui,
            "Menu button",
            "Drag the button to move it while connected",
            |c| c.menu_button.clone(),
            |c, v| c.menu_button = v,
            |h| &mut h.menu_button,
            &MENU_BUTTON_STYLES,
        );
        ui.label("");
        ui.add_enabled_ui(rows.config.menu_button_pos.is_some(), |ui| {
            if ui
                .button("Reset button position")
                .on_hover_text("Put the menu button back in the bottom-right corner")
                .clicked()
            {
                rows.config.menu_button_pos = None;
                rows.outcome.changed = true;
            }
        });
        ui.label("");
        ui.end_row();
        ui.label("Button size");
        let scale = &mut rows.config.menu_button_scale;
        if ui
            .add(
                egui::Slider::new(scale, 0.5..=2.5)
                    .step_by(0.05)
                    .custom_formatter(|v, _| format!("{:.0} %", v * 100.0))
                    .custom_parser(|s| {
                        s.trim_end_matches(['%', ' '])
                            .parse::<f64>()
                            .ok()
                            .map(|v| v / 100.0)
                    }),
            )
            .changed()
        {
            rows.outcome.changed = true;
        }
        ui.label("");
        ui.end_row();
        rows.choice(
            ui,
            "Connection warnings",
            "Explain slow streams: the network, packet loss, latency, slow decoding",
            |c| c.warnings.clone(),
            |c, v| c.warnings = v,
            |h| &mut h.warnings,
            &WARNING_STYLES,
        );
    });
}

fn app_tab(ui: &mut Ui, rows: &mut Rows, hub: Option<&mut crate::hub::HubView>) {
    // Starting at login and sharing are for desktops.
    #[cfg(not(target_os = "android"))]
    {
        let c = &mut *rows.config;
        let before = (c.autostart, c.start_hidden, c.share);
        let machine_wide = crate::autostart::machine_wide();
        grid(ui, "app", |ui| {
            ui.label("Start");
            if machine_wide {
                ui.add_enabled(
                    false,
                    egui::Checkbox::new(&mut true, "Start oriel when I log in"),
                )
                .on_disabled_hover_text("The installer starts oriel in the tray for every user");
            } else {
                ui.checkbox(&mut c.autostart, "Start oriel when I log in");
            }
            ui.label("");
            ui.end_row();
            ui.label("");
            ui.add_enabled(
                c.autostart && !machine_wide,
                egui::Checkbox::new(
                    &mut c.start_hidden,
                    if cfg!(windows) {
                        "Start in the tray"
                    } else {
                        "Start minimized"
                    },
                ),
            );
            ui.label("");
            ui.end_row();
            ui.label("Sharing");
            ui.checkbox(&mut c.share, "Share this computer when oriel starts");
            ui.label("");
            ui.end_row();
        });
        if (c.autostart, c.start_hidden, c.share) != before {
            rows.outcome.changed = true;
        }
        ui.add_space(14.0);
    }
    #[cfg(target_os = "android")]
    let _ = &rows;
    if let Some(hub) = hub {
        account_section(ui, rows, hub);
        ui.add_space(14.0);
    }
    ui.label(egui::RichText::new("About").strong());
    ui.add_space(4.0);
    grid(ui, "about", |ui| {
        for (label, value) in crate::build_info() {
            ui.label(label);
            ui.add(egui::Label::new(value).selectable(true));
            ui.label("");
            ui.end_row();
        }
    });
}

/// Narrower than this, rows stack: label above control.
const COMPACT_WIDTH: f32 = 440.0;

/// A settings grid: label, control, and a column for "Reset". In narrow
/// windows the cells stack instead.
fn grid(ui: &mut Ui, id: &str, add: impl FnOnce(&mut Ui)) {
    if ui.available_width() < COMPACT_WIDTH {
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            add(ui);
        });
        return;
    }
    egui::Grid::new(id)
        .num_columns(3)
        .spacing([16.0, 10.0])
        .min_col_width(110.0)
        .show(ui, add);
}

/// Rows that edit one setting: the default, or the host's override.
struct Rows<'a> {
    config: &'a mut ClientConfig,
    host: Option<&'a str>,
    /// Width of controls.
    field: f32,
    outcome: Outcome,
}

impl Rows<'_> {
    #[allow(clippy::too_many_arguments)]
    fn row<T: Clone + PartialEq>(
        &mut self,
        ui: &mut Ui,
        label: &str,
        hint: &str,
        default: impl Fn(&ClientConfig) -> T,
        set_default: impl Fn(&mut ClientConfig, T),
        over: impl Fn(&mut HostPrefs) -> &mut Option<T>,
        widget: impl FnOnce(&mut Ui, &mut T, f32),
    ) {
        let def = default(self.config);
        let overridden = self.host.and_then(|h| {
            let mut prefs = self.config.hosts.get(h).cloned().unwrap_or_default();
            over(&mut prefs).clone()
        });
        let response = ui.label(label);
        if !hint.is_empty() {
            response.on_hover_text(hint);
        }
        let mut value = overridden.clone().unwrap_or_else(|| def.clone());
        let before = value.clone();
        widget(ui, &mut value, self.field);
        if value != before {
            match self.host {
                // Matching the default is no override.
                Some(h) => *over(self.config.host_mut(h)) = (value != def).then_some(value),
                None => set_default(self.config, value),
            }
            self.outcome.changed = true;
        }
        match (self.host, &overridden) {
            (Some(h), Some(_)) => {
                if ui
                    .small_button("Reset")
                    .on_hover_text("Use the default for all computers")
                    .clicked()
                {
                    *over(self.config.host_mut(h)) = None;
                    self.outcome.changed = true;
                }
            }
            _ => {
                ui.label("");
            }
        }
        ui.end_row();
    }

    #[allow(clippy::too_many_arguments)]
    fn toggle(
        &mut self,
        ui: &mut Ui,
        label: &str,
        text: &str,
        hint: &str,
        default: impl Fn(&ClientConfig) -> bool,
        set_default: impl Fn(&mut ClientConfig, bool),
        over: impl Fn(&mut HostPrefs) -> &mut Option<bool>,
    ) {
        self.row(ui, label, hint, default, set_default, over, |ui, v, _| {
            ui.checkbox(v, text);
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn choice(
        &mut self,
        ui: &mut Ui,
        label: &str,
        hint: &str,
        default: impl Fn(&ClientConfig) -> String,
        set_default: impl Fn(&mut ClientConfig, String),
        over: impl Fn(&mut HostPrefs) -> &mut Option<String>,
        choices: &[(&str, &str)],
    ) {
        self.row(ui, label, hint, default, set_default, over, |ui, v, w| {
            combo(ui, label, v, choices, w)
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn mbps(
        &mut self,
        ui: &mut Ui,
        label: &str,
        hint: &str,
        range: std::ops::RangeInclusive<u32>,
        default: impl Fn(&ClientConfig) -> u32,
        set_default: impl Fn(&mut ClientConfig, u32),
        over: impl Fn(&mut HostPrefs) -> &mut Option<u32>,
    ) {
        self.row(
            ui,
            label,
            hint,
            default,
            set_default,
            over,
            |ui, kbps, _| {
                let mut mbps = *kbps / 1000;
                let slider = ui.add(
                    egui::Slider::new(&mut mbps, range)
                        .logarithmic(true)
                        .suffix(" Mbit/s"),
                );
                // Apply once dragging ends, not on every step.
                if slider.drag_stopped() || (slider.changed() && !slider.dragged()) {
                    *kbps = mbps * 1000;
                }
            },
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn device(
        &mut self,
        ui: &mut Ui,
        label: &str,
        default_name: &str,
        devices: &[(String, String)],
        default: impl Fn(&ClientConfig) -> Option<String>,
        set_default: impl Fn(&mut ClientConfig, Option<String>),
        over: impl Fn(&mut HostPrefs) -> &mut Option<String>,
    ) {
        // Overrides store "" for the default device.
        self.row(
            ui,
            label,
            "",
            |c| default(c).unwrap_or_default(),
            |c, v| set_default(c, Some(v).filter(|v| !v.is_empty())),
            over,
            |ui, v, w| {
                let name = |id: &str| {
                    if id.is_empty() {
                        default_name.to_string()
                    } else {
                        devices
                            .iter()
                            .find(|(i, _)| i == id)
                            .map_or_else(|| format!("{id} (not found)"), |(_, n)| n.clone())
                    }
                };
                egui::ComboBox::from_id_salt(label)
                    .width(w)
                    .selected_text(name(v))
                    .show_ui(ui, |ui| {
                        for id in std::iter::once("").chain(devices.iter().map(|(i, _)| i.as_str()))
                        {
                            if ui.selectable_label(v == id, name(id)).clicked() {
                                *v = id.to_string();
                            }
                        }
                    });
            },
        );
    }
}

fn combo(ui: &mut Ui, id: &str, value: &mut String, choices: &[(&str, &str)], width: f32) {
    let label = choices
        .iter()
        .find(|(k, _)| k == value)
        .map_or(value.as_str(), |(_, l)| *l)
        .to_string();
    egui::ComboBox::from_id_salt(id)
        .width(width)
        .selected_text(label)
        .show_ui(ui, |ui| {
            for (key, label) in choices {
                if ui.selectable_label(value == key, *label).clicked() {
                    *value = key.to_string();
                }
            }
        });
}

/// The scale slider of "This screen". Returns a new scale once dragging
/// ends: each change switches the host's display mode.
fn scale_row(ui: &mut Ui, scale: f32) -> Option<f32> {
    ui.label("Scale")
        .on_hover_text("Lower resolutions are lighter on the network and both computers");
    let id = ui.make_persistent_id("resolution-scale");
    let mut v = ui.data(|d| d.get_temp::<f32>(id)).unwrap_or(scale);
    let slider = ui.add(
        egui::Slider::new(&mut v, crate::config::RESOLUTION_SCALE)
            .step_by(0.05)
            .custom_formatter(|v, _| format!("{:.0} %", v * 100.0))
            .custom_parser(|s| {
                s.trim_end_matches(['%', ' '])
                    .parse::<f64>()
                    .ok()
                    .map(|v| v / 100.0)
            }),
    );
    let changed = if slider.dragged() {
        ui.data_mut(|d| d.insert_temp(id, v));
        None
    } else {
        ui.data_mut(|d| d.remove::<f32>(id));
        ((v - scale).abs() > 0.001).then_some(v)
    };
    ui.label("");
    ui.end_row();
    changed
}

fn save_resolution(rows: &mut Rows, host: &str, value: Resolution) {
    let h = rows.config.host_mut(host);
    match value {
        Resolution::Host => h.resolution = None,
        Resolution::Client(scale) => {
            h.resolution = Some("client".into());
            h.resolution_scale = Some(scale);
        }
        Resolution::Fixed(w, h2) => h.resolution = Some(format_resolution(Some((w, h2)))),
    }
    rows.outcome.changed = true;
}

/// The optional hub: log in once per device, and every device of the
/// account reaches every computer shared under it, without pairing.
fn account_section(ui: &mut Ui, rows: &mut Rows, hub: &mut crate::hub::HubView) {
    use crate::hub::HubAction;
    let act = |rows: &mut Rows, a: HubAction| rows.outcome.actions.push(LiveAction::Hub(a));
    ui.label(egui::RichText::new("Account").strong());
    ui.add_space(4.0);
    if let Some(user) = hub.user.clone() {
        ui.horizontal_wrapped(|ui| {
            ui.label(format!("Logged in to {} as {user}", hub.url));
            if ui.button("Log out").clicked() {
                act(rows, HubAction::Logout);
            }
        });
        ui.add_space(4.0);
        theme::note(
            ui,
            "Every device logged in to this account can reach the computers shared under it.",
        );
        ui.add_space(6.0);
        grid(ui, "hub-devices", |ui| {
            for d in &hub.devices {
                ui.horizontal(|ui| {
                    let color = if d.online { theme::GREEN } else { theme::MUTED };
                    let (dot, _) =
                        ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                    ui.painter().circle_filled(dot.center(), 4.0, color);
                    ui.label(&d.name);
                });
                let mut what = vec![d.platform.clone()];
                if d.host.is_some() {
                    what.push("shares".into());
                }
                if d.this_device {
                    what.push("this device".into());
                }
                ui.label(egui::RichText::new(what.join(" · ")).color(theme::MUTED));
                ui.horizontal(|ui| {
                    if d.wakeable
                        && !d.online
                        && ui
                            .small_button("Wake")
                            .on_hover_text("Ask it to start sharing")
                            .clicked()
                    {
                        act(rows, HubAction::Wake(d.id.clone()));
                    }
                    if !d.this_device
                        && ui
                            .small_button("Remove")
                            .on_hover_text("Log this device out of the account")
                            .clicked()
                    {
                        act(rows, HubAction::Remove(d.id.clone()));
                    }
                });
                ui.end_row();
            }
        });
    } else if let Some(login) = hub.login.clone() {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Waiting for you to approve this device…");
        });
        ui.add_space(4.0);
        ui.label(format!("Open {} and enter", login.uri));
        ui.label(
            egui::RichText::new(&login.code)
                .size(26.0)
                .monospace()
                .strong(),
        );
        ui.horizontal(|ui| {
            if ui.add(theme_primary("Open sign-in page")).clicked() {
                act(rows, HubAction::Open(login.open.clone()));
            }
            if ui.button("Cancel").clicked() {
                act(rows, HubAction::Cancel);
            }
        });
    } else {
        theme::note(
            ui,
            "Optional: log in to a hub to reach your computers from every device you log in \
             on, without pairing each one, and through the hub when away from home.",
        );
        ui.add_space(6.0);
        grid(ui, "hub-login", |ui| {
            ui.label("Hub");
            ui.add(
                egui::TextEdit::singleline(&mut hub.url)
                    .hint_text(crate::hub::DEFAULT_URL)
                    .desired_width(rows.field),
            );
            if ui.button("Log in").clicked() {
                act(rows, HubAction::Login);
            }
            ui.end_row();
        });
    }
    if let Some(error) = &hub.error {
        ui.add_space(4.0);
        ui.add(egui::Label::new(egui::RichText::new(error).color(theme::AMBER)).wrap());
    }
}

fn theme_primary(text: &str) -> egui::Button<'static> {
    egui::Button::new(egui::RichText::new(text.to_string()).color(egui::Color32::BLACK))
        .fill(theme::AMBER)
}
