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
    pub max_fps: u32,
    /// Most preferred first: "hevc", "h264", "av1", "pyrowave", or just
    /// "auto" (PyroWave when the link allows, else HEVC/H.264).
    pub codecs: Vec<String>,
    pub pyrowave_kbps: u32,
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
    pub pyrowave_kbps: Option<u32>,
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
    // Only meaningful per host:
    /// "native" or "WIDTHxHEIGHT".
    pub resolution: Option<String>,
    /// The host output to hear; unset follows the host's default.
    pub audio_device: Option<String>,
    /// Use this window as an extra display of the host, "WIDTHxHEIGHT@HZ".
    pub virtual_display: Option<String>,
}

/// The settings in force for one host: its overrides over the defaults.
#[derive(Debug, Clone, PartialEq)]
pub struct Effective {
    pub fullscreen: bool,
    pub codec: String,
    pub bitrate_kbps: u32,
    pub pyrowave_kbps: u32,
    pub max_fps: u32,
    pub mouse: String,
    pub gamepads: bool,
    pub audio: bool,
    pub playback_device: Option<String>,
    pub microphone: bool,
    pub microphone_device: Option<String>,
    pub show_stats: bool,
    pub menu_button: String,
    pub resolution: Option<(u32, u32)>,
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
            max_fps: 120,
            codecs: vec!["hevc".into(), "h264".into()],
            pyrowave_kbps: 400_000,
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
            pyrowave_kbps: h.pyrowave_kbps.unwrap_or(self.pyrowave_kbps),
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
            resolution: h.resolution.as_deref().and_then(parse_resolution),
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
        assert_eq!(e.resolution, Some((2560, 1440)));
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
