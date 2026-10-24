//! Optional client settings in `client.toml` next to the identity.

use anyhow::{Context, Result, bail};
use oriel_net::Paths;
use oriel_proto::Codec;
use serde::{Deserialize, Serialize};

use crate::{
    MouseMode,
    keymap::{Bindings, Command, Shortcut},
};

pub const AUTO_CODECS: [Codec; 3] = [Codec::PyroWave, Codec::Hevc, Codec::H264];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClientConfig {
    /// Opens the menu and releases keyboard and mouse.
    pub menu_shortcut: String,
    pub bitrate_kbps: u32,
    /// Lower the bitrate (below `bitrate_kbps`) while the network cannot
    /// carry it.
    pub auto_bitrate: bool,
    /// Hold frames briefly to even out network jitter: "off" (lowest
    /// latency), "auto", or milliseconds.
    pub smoothing: String,
    pub max_fps: u32,
    /// Most preferred first: "hevc", "h264", "av1", "pyrowave", or just
    /// "auto" (PyroWave when the link allows, else HEVC/H.264).
    pub codecs: Vec<String>,
    pub pyrowave_kbps: u32,
    /// Ask for colour at full resolution (4:4:4) where the host's encoder
    /// can: sharper coloured text, more bandwidth.
    pub full_chroma: bool,
    /// Decode on this computer's GPU ("hardware", falling back to software
    /// where it cannot) or its CPU ("software").
    pub decoding: String,
    /// The same for encoding on the host.
    pub encoding: String,
    /// "auto", "desktop" or "captured".
    pub mouse: String,
    pub audio: bool,
    pub fullscreen: bool,
    pub show_stats: bool,
    /// Share this computer when oriel starts.
    pub share: bool,
    /// Start oriel when the user logs in (mirrors the OS's autostart
    /// entry, see `crate::autostart`).
    pub autostart: bool,
    /// When started at login, stay in the tray (Windows) or minimized.
    pub start_hidden: bool,
    /// Local output for remote sound (an `oriel_audio::OutputDevice` id);
    /// unset plays on the default device.
    pub playback_device: Option<String>,
    /// Forward gamepads to the host while the viewer has focus.
    pub gamepads: bool,
    /// The menu button: "full" (logo and shortcut), "shortcut", "logo" or
    /// "hidden" (the shortcut still works).
    pub menu_button: String,
    /// Where the menu button's center sits, as fractions of the window
    /// (unset: bottom right).
    pub menu_button_pos: Option<[f32; 2]>,
    /// The menu button's size, 1 being the default.
    pub menu_button_scale: f32,
    /// Warnings that explain a slow stream: "detailed" (with what to try),
    /// "short" or "off".
    pub warnings: String,
    /// Send the microphone to hosts by default.
    pub microphone: bool,
    /// Which input device (an `oriel_audio` device id; unset: default).
    pub microphone_device: Option<String>,
    /// Shortcuts for the other viewer commands, by command id (see
    /// [`Command::id`]): a missing entry uses the default, an empty one
    /// disables the shortcut.
    pub shortcuts: std::collections::BTreeMap<String, String>,
    /// Remembered per-host choices, by host name.
    pub hosts: std::collections::BTreeMap<String, HostPrefs>,
}

/// Settings for one host that differ from the defaults; unset fields use
/// the defaults (see [`ClientConfig::effective`]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HostPrefs {
    pub fullscreen: Option<bool>,
    /// A codec choice key (see [`codec_choice`]).
    pub codec: Option<String>,
    pub bitrate_kbps: Option<u32>,
    pub auto_bitrate: Option<bool>,
    pub smoothing: Option<String>,
    pub pyrowave_kbps: Option<u32>,
    pub full_chroma: Option<bool>,
    pub decoding: Option<String>,
    pub encoding: Option<String>,
    pub max_fps: Option<u32>,
    pub mouse: Option<String>,
    pub gamepads: Option<bool>,
    pub audio: Option<bool>,
    /// Local output ("" for the default device).
    pub playback_device: Option<String>,
    pub microphone: Option<bool>,
    /// Input device ("" for the default one).
    pub microphone_device: Option<String>,
    pub show_stats: Option<bool>,
    pub menu_button: Option<String>,
    pub warnings: Option<String>,
    // Only meaningful per host:
    /// "native", "client" (this computer's screen, times
    /// `resolution_scale`) or "WIDTHxHEIGHT".
    pub resolution: Option<String>,
    /// The host output to hear; unset follows the host's default.
    pub audio_device: Option<String>,
    /// Use this window as an extra display of the host, "WIDTHxHEIGHT@HZ".
    pub virtual_display: Option<String>,
    /// For the "client" resolution: 0.1–1.
    pub resolution_scale: Option<f32>,
}

/// The settings in force for one host: its overrides over the defaults.
#[derive(Debug, Clone, PartialEq)]
pub struct Effective {
    pub fullscreen: bool,
    pub codec: String,
    pub bitrate_kbps: u32,
    pub auto_bitrate: bool,
    pub smoothing: String,
    pub pyrowave_kbps: u32,
    pub full_chroma: bool,
    pub decoding: String,
    pub encoding: String,
    pub max_fps: u32,
    pub mouse: String,
    pub gamepads: bool,
    pub audio: bool,
    pub playback_device: Option<String>,
    pub microphone: bool,
    pub microphone_device: Option<String>,
    pub show_stats: bool,
    pub menu_button: String,
    pub warnings: String,
    pub resolution: Resolution,
    pub audio_device: Option<String>,
    pub virtual_display: Option<oriel_proto::VirtualMode>,
}

impl Effective {
    pub fn mouse_mode(&self) -> MouseMode {
        match self.mouse.as_str() {
            "desktop" => MouseMode::Absolute,
            "captured" => MouseMode::Relative,
            _ => MouseMode::Auto,
        }
    }
}

/// A device choice where "" means the default device.
fn device(choice: Option<&String>, default: &Option<String>) -> Option<String> {
    match choice {
        Some(d) if d.is_empty() => None,
        Some(d) => Some(d.clone()),
        None => default.clone(),
    }
}

/// Mouse modes offered in the UI, as (key, label).
pub const MOUSE_MODES: [(&str, &str); 3] = [
    ("auto", "Automatic"),
    ("desktop", "Desktop"),
    ("captured", "Captured (games)"),
];

/// The `codecs` list stored in client.toml for a choice key.
pub fn codec_choice_strings(key: &str) -> Vec<String> {
    match key {
        "auto" => vec!["auto".into()],
        "h264" => vec!["h264".into()],
        "av1" => vec!["av1".into(), "hevc".into(), "h264".into()],
        "pyrowave" => vec!["pyrowave".into(), "hevc".into(), "h264".into()],
        _ => vec!["hevc".into(), "h264".into()],
    }
}

pub fn parse_virtual_mode(s: &str) -> Option<oriel_proto::VirtualMode> {
    let (size, hz) = s.split_once('@')?;
    let (width, height) = parse_resolution(size)?;
    Some(oriel_proto::VirtualMode {
        width,
        height,
        refresh_hz: hz.trim().parse().ok()?,
    })
}

pub fn format_virtual_mode(m: &oriel_proto::VirtualMode) -> String {
    format!("{}x{}@{}", m.width, m.height, m.refresh_hz)
}

/// Menu button styles offered in the UI, as (key, label).
pub const MENU_BUTTON_STYLES: [(&str, &str); 4] = [
    ("full", "Logo and shortcut"),
    ("shortcut", "Shortcut only"),
    ("logo", "Logo only"),
    ("hidden", "Hidden (use the shortcut)"),
];

/// Where to decode or encode, as (key, label).
pub const CODER_CHOICES: [(&str, &str); 2] = [
    ("hardware", "Hardware (GPU)"),
    ("software", "Software (CPU)"),
];

/// Smoothing choices, as (key, label).
pub const SMOOTHING_CHOICES: [(&str, &str); 8] = [
    ("off", "Off (lowest latency)"),
    ("auto", "Automatic (as jitter needs)"),
    ("10", "10 ms"),
    ("20", "20 ms"),
    ("35", "35 ms"),
    ("50", "50 ms"),
    ("75", "75 ms"),
    ("100", "100 ms"),
];

/// How connection warnings are shown, as (key, label).
pub const WARNING_STYLES: [(&str, &str); 3] = [
    ("detailed", "Detailed, with what to try"),
    ("short", "Short"),
    ("off", "Off"),
];

/// The codec choices offered in the UI, as (key, label).
pub const CODEC_CHOICES: [(&str, &str); 5] = [
    ("auto", "Automatic"),
    ("hevc", "HEVC"),
    ("h264", "H.264"),
    ("av1", "AV1"),
    ("pyrowave", "PyroWave"),
];

/// The preference list for a codec choice key, and whether the host may
/// switch automatically.
pub fn codec_choice(key: &str) -> (Vec<Codec>, bool) {
    match key {
        "auto" => (AUTO_CODECS.to_vec(), true),
        "h264" => (vec![Codec::H264], false),
        "av1" => (vec![Codec::Av1, Codec::Hevc, Codec::H264], false),
        "pyrowave" => (vec![Codec::PyroWave, Codec::Hevc, Codec::H264], false),
        _ => (vec![Codec::Hevc, Codec::H264], false),
    }
}

/// The resolution to ask the host's display for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Resolution {
    /// Leave the host's display as it is.
    Host,
    /// This computer's screen, scaled (0.1–1).
    Client(f32),
    Fixed(u32, u32),
}

pub const RESOLUTION_SCALE: std::ops::RangeInclusive<f32> = 0.1..=1.0;

impl Resolution {
    pub fn parse(s: Option<&str>, scale: Option<f32>) -> Self {
        match s {
            Some("client") => Self::Client(
                scale
                    .unwrap_or(1.0)
                    .clamp(*RESOLUTION_SCALE.start(), *RESOLUTION_SCALE.end()),
            ),
            Some(s) => parse_resolution(s).map_or(Self::Host, |(w, h)| Self::Fixed(w, h)),
            None => Self::Host,
        }
    }

    /// The size to ask for, given this computer's screen (physical pixels).
    pub fn size(self, screen: Option<(u32, u32)>) -> Option<(u32, u32)> {
        match self {
            Self::Host => None,
            Self::Fixed(w, h) => Some((w, h)),
            Self::Client(scale) => {
                let (w, h) = screen?;
                // Even sizes, which encoders need.
                let even = |v: u32| ((v as f32 * scale / 2.0).round() as u32 * 2).max(2);
                Some((even(w), even(h)))
            }
        }
    }
}

pub fn parse_resolution(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.split_once(['x', '×'])?;
    Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
}

pub fn format_resolution(r: Option<(u32, u32)>) -> String {
    match r {
        Some((w, h)) => format!("{w}x{h}"),
        None => "native".into(),
    }
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            menu_shortcut: "Ctrl+Alt+Shift+M".into(),
            bitrate_kbps: 50_000,
            auto_bitrate: true,
            smoothing: "off".into(),
            max_fps: 120,
            codecs: vec!["hevc".into(), "h264".into()],
            pyrowave_kbps: 400_000,
            full_chroma: false,
            decoding: "hardware".into(),
            encoding: "hardware".into(),
            mouse: "auto".into(),
            audio: true,
            fullscreen: false,
            show_stats: false,
            share: false,
            autostart: false,
            start_hidden: true,
            playback_device: None,
            gamepads: true,
            menu_button: "full".into(),
            menu_button_pos: None,
            menu_button_scale: 1.0,
            warnings: "detailed".into(),
            microphone: false,
            microphone_device: None,
            shortcuts: Default::default(),
            hosts: Default::default(),
        }
    }
}

impl ClientConfig {
    /// The settings in force for `host` (just the defaults without one).
    pub fn effective(&self, host: Option<&str>) -> Effective {
        let empty = HostPrefs::default();
        let h = host.and_then(|h| self.hosts.get(h)).unwrap_or(&empty);
        Effective {
            fullscreen: h.fullscreen.unwrap_or(self.fullscreen),
            codec: h.codec.clone().unwrap_or_else(|| self.codec_key()),
            bitrate_kbps: h.bitrate_kbps.unwrap_or(self.bitrate_kbps),
            auto_bitrate: h.auto_bitrate.unwrap_or(self.auto_bitrate),
            smoothing: h
                .smoothing
                .clone()
                .unwrap_or_else(|| self.smoothing.clone()),
            pyrowave_kbps: h.pyrowave_kbps.unwrap_or(self.pyrowave_kbps),
            full_chroma: h.full_chroma.unwrap_or(self.full_chroma),
            decoding: h.decoding.clone().unwrap_or_else(|| self.decoding.clone()),
            encoding: h.encoding.clone().unwrap_or_else(|| self.encoding.clone()),
            max_fps: h.max_fps.unwrap_or(self.max_fps),
            mouse: h.mouse.clone().unwrap_or_else(|| self.mouse.clone()),
            gamepads: h.gamepads.unwrap_or(self.gamepads),
            audio: h.audio.unwrap_or(self.audio),
            playback_device: device(h.playback_device.as_ref(), &self.playback_device),
            microphone: h.microphone.unwrap_or(self.microphone),
            microphone_device: device(h.microphone_device.as_ref(), &self.microphone_device),
            show_stats: h.show_stats.unwrap_or(self.show_stats),
            menu_button: h
                .menu_button
                .clone()
                .unwrap_or_else(|| self.menu_button.clone()),
            warnings: h.warnings.clone().unwrap_or_else(|| self.warnings.clone()),
            resolution: Resolution::parse(h.resolution.as_deref(), h.resolution_scale),
            audio_device: h.audio_device.clone().filter(|d| !d.is_empty()),
            virtual_display: h.virtual_display.as_deref().and_then(parse_virtual_mode),
        }
    }

    /// The overrides for `host`, created if needed.
    pub fn host_mut(&mut self, host: &str) -> &mut HostPrefs {
        self.hosts.entry(host.to_string()).or_default()
    }

    /// The configured shortcuts; unparsable entries fall back to defaults.
    pub fn bindings(&self) -> Bindings {
        let mut bindings = Bindings::default();
        for command in Command::ALL {
            let text = match command {
                Command::Menu => Some(&self.menu_shortcut),
                _ => self.shortcuts.get(command.id()),
            };
            let Some(text) = text else { continue };
            if text.trim().is_empty() && command != Command::Menu {
                bindings.set(command, None);
            } else if let Some(s) = Shortcut::parse(text) {
                bindings.set(command, Some(s));
            } else {
                tracing::warn!("ignoring unknown shortcut {text:?} for {}", command.id());
            }
        }
        bindings
    }

    /// Store `bindings`, keeping only entries that differ from the default.
    pub fn set_bindings(&mut self, bindings: &Bindings) {
        self.menu_shortcut = bindings.menu().to_string();
        for command in Command::ALL {
            if command == Command::Menu {
                continue;
            }
            let text = bindings
                .get(command)
                .map(|s| s.to_string())
                .unwrap_or_default();
            let default = Shortcut::parse(command.default_shortcut()).map(|s| s.to_string());
            if Some(&text) == default.as_ref() {
                self.shortcuts.remove(command.id());
            } else {
                self.shortcuts.insert(command.id().into(), text);
            }
        }
    }

    pub fn path(paths: &Paths) -> std::path::PathBuf {
        paths.config_dir.join("client.toml")
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        std::fs::create_dir_all(&paths.config_dir)?;
        std::fs::write(Self::path(paths), toml::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn load(paths: &Paths) -> Result<Self> {
        let path = Self::path(paths);
        match std::fs::read_to_string(&path) {
            Ok(s) => toml::from_str(&s).with_context(|| format!("parse {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
        }
    }

    /// The codec preference list and whether automatic switching is on.
    pub fn codecs(&self) -> Result<(Vec<Codec>, bool)> {
        if self.codecs.iter().any(|c| c.eq_ignore_ascii_case("auto")) {
            return Ok((AUTO_CODECS.to_vec(), true));
        }
        let list = self
            .codecs
            .iter()
            .map(|c| {
                Ok(match c.to_ascii_lowercase().as_str() {
                    "h264" | "avc" => Codec::H264,
                    "hevc" | "h265" => Codec::Hevc,
                    "av1" => Codec::Av1,
                    "pyrowave" => Codec::PyroWave,
                    other => bail!("unknown codec {other:?} in client.toml"),
                })
            })
            .collect::<Result<_>>()?;
        Ok((list, false))
    }

    /// The global codec choice key.
    pub fn codec_key(&self) -> String {
        if self.codecs.iter().any(|c| c.eq_ignore_ascii_case("auto")) {
            return "auto".into();
        }
        self.codecs
            .first()
            .map(|c| c.to_ascii_lowercase())
            .filter(|c| CODEC_CHOICES.iter().any(|(k, _)| k == c))
            .unwrap_or_else(|| "hevc".into())
    }

    pub fn mouse_mode(&self) -> Result<MouseMode> {
        Ok(match self.mouse.as_str() {
            "auto" => MouseMode::Auto,
            "desktop" => MouseMode::Absolute,
            "captured" => MouseMode::Relative,
            other => bail!("unknown mouse mode {other:?} in client.toml"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_overrides() {
        let mut c = ClientConfig {
            max_fps: 60,
            playback_device: Some("speakers".into()),
            ..Default::default()
        };
        assert_eq!(c.effective(Some("pc")).max_fps, 60);
        let h = c.host_mut("pc");
        h.max_fps = Some(144);
        // "" overrides a default device with the system default.
        h.playback_device = Some(String::new());
        h.resolution = Some("2560x1440".into());
        let e = c.effective(Some("pc"));
        assert_eq!(e.max_fps, 144);
        assert_eq!(e.playback_device, None);
        assert_eq!(e.resolution, Resolution::Fixed(2560, 1440));
        let h = c.host_mut("pc");
        h.resolution = Some("client".into());
        h.resolution_scale = Some(0.5);
        let e = c.effective(Some("pc"));
        assert_eq!(e.resolution, Resolution::Client(0.5));
        assert_eq!(e.resolution.size(Some((2560, 1600))), Some((1280, 800)));
        assert_eq!(e.resolution.size(None), None);
        assert_eq!(
            Resolution::Client(0.1).size(Some((1366, 768))),
            Some((136, 76))
        );
        // Other hosts and the defaults are unaffected.
        assert_eq!(c.effective(Some("other")).max_fps, 60);
        assert_eq!(
            c.effective(None).playback_device.as_deref(),
            Some("speakers")
        );
        let c: ClientConfig = toml::from_str(&toml::to_string(&c).unwrap()).unwrap();
        assert_eq!(c.effective(Some("pc")).max_fps, 144);
    }

    #[test]
    fn bindings_roundtrip() {
        let mut c = ClientConfig::default();
        assert_eq!(c.bindings(), Bindings::default());
        let mut b = c.bindings();
        b.set(Command::Stats, None);
        b.set(Command::NextDisplay, Shortcut::parse("Alt+PageDown"));
        c.set_bindings(&b);
        // Only the changes are stored.
        assert_eq!(c.shortcuts.len(), 2);
        assert_eq!(c.shortcuts["stats"], "");
        let c: ClientConfig = toml::from_str(&toml::to_string(&c).unwrap()).unwrap();
        let b = c.bindings();
        assert_eq!(b.get(Command::Stats), None);
        assert_eq!(b.get(Command::NextDisplay), Shortcut::parse("Alt+PageDown"));
        assert_eq!(b.menu().to_string(), "Ctrl+Alt+Shift+M");
    }
}
