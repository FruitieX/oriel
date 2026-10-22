//! Optional client settings in `client.toml` next to the identity.

use anyhow::{Context, Result, bail};
use oriel_client::MouseMode;
use oriel_net::Paths;
use oriel_proto::Codec;
use serde::Deserialize;

pub const AUTO_CODECS: [Codec; 3] = [Codec::PyroWave, Codec::Hevc, Codec::H264];

#[derive(Debug, Deserialize)]
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
        }
    }
}

impl ClientConfig {
    pub fn load(paths: &Paths) -> Result<Self> {
        let path = paths.config_dir.join("client.toml");
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

    pub fn mouse_mode(&self) -> Result<MouseMode> {
        Ok(match self.mouse.as_str() {
            "auto" => MouseMode::Auto,
            "desktop" => MouseMode::Absolute,
            "captured" => MouseMode::Relative,
            other => bail!("unknown mouse mode {other:?} in client.toml"),
        })
    }
}
