//! Optional client settings in `client.toml` next to the identity.

use anyhow::{Context, Result, bail};
use oriel_net::Paths;
use oriel_proto::Codec;
use serde::{Deserialize, Serialize};

use crate::MouseMode;

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
    /// Remembered per-host choices, by host name.
    pub hosts: std::collections::BTreeMap<String, HostPrefs>,
}

/// Choices remembered for one host; unset fields use the global settings.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HostPrefs {
    pub fullscreen: Option<bool>,
    /// A codec choice key (see [`codec_choice`]).
    pub codec: Option<String>,
    /// "native" or "WIDTHxHEIGHT".
    pub resolution: Option<String>,
}

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
            hosts: Default::default(),
        }
    }
}

impl ClientConfig {
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
