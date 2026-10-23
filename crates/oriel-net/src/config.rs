use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::Fingerprint;

/// Where oriel keeps its state.
#[derive(Debug, Clone)]
pub struct Paths {
    pub config_dir: PathBuf,
}

impl Paths {
    /// `ORIEL_CONFIG_DIR` overrides the platform default (useful for running
    /// a host and a client side by side, and for the Windows service).
    pub fn discover() -> Result<Self> {
        if let Some(dir) = std::env::var_os("ORIEL_CONFIG_DIR") {
            return Ok(Self {
                config_dir: dir.into(),
            });
        }
        let dirs = directories::ProjectDirs::from("org", "fruitiex", "oriel")
            .context("no home directory to store configuration in")?;
        Ok(Self {
            config_dir: dirs.config_dir().to_path_buf(),
        })
    }

    pub fn trusted_clients(&self) -> PathBuf {
        self.config_dir.join("trusted-clients.toml")
    }

    pub fn known_hosts(&self) -> PathBuf {
        self.config_dir.join("known-hosts.toml")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustedPeer {
    pub name: String,
    pub fingerprint: Fingerprint,
    /// Last address used to reach a host (unused for clients).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
}

/// A TOML list of trusted peers: paired clients on a host, known hosts on a
/// client.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct TrustStore {
    #[serde(default, rename = "peer")]
    pub peers: Vec<TrustedPeer>,
}

impl TrustStore {
    pub fn load(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(s) => toml::from_str(&s).with_context(|| format!("parse {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, toml::to_string_pretty(self)?)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn by_fingerprint(&self, fp: &Fingerprint) -> Option<&TrustedPeer> {
        self.peers.iter().find(|p| &p.fingerprint == fp)
    }

    /// Find a peer by name (case-insensitive) or by address.
    pub fn lookup(&self, name_or_addr: &str) -> Option<&TrustedPeer> {
        self.peers
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case(name_or_addr))
            .or_else(|| {
                self.peers
                    .iter()
                    .find(|p| p.address.as_deref() == Some(name_or_addr))
            })
    }

    /// Insert or replace (by fingerprint, then by name).
    pub fn upsert(&mut self, peer: TrustedPeer) {
        self.peers.retain(|p| {
            p.fingerprint != peer.fingerprint && !p.name.eq_ignore_ascii_case(&peer.name)
        });
        self.peers.push(peer);
    }

    /// Add a peer that may share another's name (two devices that call
    /// themselves the same): replace only the same fingerprint, and number
    /// the name if it is taken ("laptop (2)").
    pub fn add_distinct(&mut self, mut peer: TrustedPeer) -> String {
        self.peers.retain(|p| p.fingerprint != peer.fingerprint);
        let base = peer.name.clone();
        let mut n = 2;
        while self
            .peers
            .iter()
            .any(|p| p.name.eq_ignore_ascii_case(&peer.name))
        {
            peer.name = format!("{base} ({n})");
            n += 1;
        }
        let name = peer.name.clone();
        self.peers.push(peer);
        name
    }

    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.peers.len();
        self.peers.retain(|p| !p.name.eq_ignore_ascii_case(name));
        before != self.peers.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(name: &str, fp: u8) -> TrustedPeer {
        TrustedPeer {
            name: name.into(),
            fingerprint: Fingerprint([fp; 32]),
            address: None,
        }
    }

    #[test]
    fn same_named_clients_both_stay() {
        let mut store = TrustStore::default();
        assert_eq!(store.add_distinct(peer("oriel-client", 1)), "oriel-client");
        assert_eq!(
            store.add_distinct(peer("oriel-client", 2)),
            "oriel-client (2)"
        );
        assert_eq!(
            store.add_distinct(peer("Oriel-Client", 3)),
            "Oriel-Client (3)"
        );
        assert_eq!(store.peers.len(), 3);
        // Pairing the same device again replaces its entry.
        assert_eq!(store.add_distinct(peer("phone", 2)), "phone");
        assert_eq!(store.peers.len(), 3);
        assert!(store.by_fingerprint(&Fingerprint([1; 32])).is_some());
    }
}
