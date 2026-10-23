//! The optional hub (see `oriel_proto::hub`): this device's login, the
//! account's devices, relays. Calls are blocking (ureq): run them on a
//! blocking thread.
//!
//! The login lives in `hub.toml` in the config directory. The Windows
//! service also looks in its `pairing` directory, which the app may write.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use oriel_proto::hub::*;
use serde::{Deserialize, Serialize};

use crate::{Fingerprint, Paths, TrustStore, TrustedPeer};

/// This device's hub login.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Account {
    /// e.g. https://oriel.fruitiex.org
    pub url: String,
    pub token: String,
    pub device_id: String,
    /// Who logged in.
    pub user: String,
}

impl Paths {
    pub fn hub_account(&self) -> PathBuf {
        self.config_dir.join("hub.toml")
    }

    /// The account's devices as last fetched.
    pub fn hub_devices(&self) -> PathBuf {
        self.config_dir.join("hub-devices.json")
    }
}

/// Where else a login may be: the Windows service's pairing directory,
/// which the signed-in user's app can write.
fn shared_account(paths: &Paths) -> PathBuf {
    paths.config_dir.join("pairing").join("hub.toml")
}

pub fn account(paths: &Paths) -> Option<Account> {
    [paths.hub_account(), shared_account(paths)]
        .iter()
        .find_map(|p| toml::from_str(&std::fs::read_to_string(p).ok()?).ok())
}

pub fn save_account(path: &Path, account: &Account) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, toml::to_string_pretty(account)?)
        .with_context(|| format!("write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

pub fn forget_account(paths: &Paths) {
    let _ = std::fs::remove_file(paths.hub_account());
    let _ = std::fs::remove_file(paths.hub_devices());
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(10)))
        .http_status_as_error(false)
        .build()
        .into()
}

fn url(base: &str, path: &str) -> String {
    format!("{}{path}", base.trim_end_matches('/'))
}

fn read<T: serde::de::DeserializeOwned>(mut r: ureq::http::Response<ureq::Body>) -> Result<T> {
    let status = r.status();
    if status.is_success() {
        return Ok(r.body_mut().read_json()?);
    }
    let error = r
        .body_mut()
        .read_json::<ApiError>()
        .map(|e| e.error)
        .unwrap_or_else(|_| status.to_string());
    if status == 401 {
        bail!(Unauthorized(error));
    }
    bail!("{error}")
}

/// The hub no longer accepts this device's login (removed or replaced).
#[derive(Debug)]
pub struct Unauthorized(pub String);

impl std::fmt::Display for Unauthorized {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "logged out of the hub: {}", self.0)
    }
}

impl std::error::Error for Unauthorized {}

pub fn login_start(hub: &str, fingerprint: &Fingerprint, name: &str) -> Result<LoginStarted> {
    let r = agent()
        .post(url(hub, "/api/login/start"))
        .send_json(LoginStart {
            fingerprint: hex::encode(fingerprint.0),
            name: name.to_string(),
            platform: std::env::consts::OS.to_string(),
        })
        .with_context(|| format!("reach {hub}"))?;
    read(r)
}

pub fn login_poll(hub: &str, login_id: &str) -> Result<LoginStatus> {
    let r = agent()
        .post(url(hub, "/api/login/poll"))
        .send_json(LoginPoll {
            login_id: login_id.to_string(),
        })?;
    read(r)
}

impl Account {
    fn get(&self, path: &str) -> Result<ureq::http::Response<ureq::Body>> {
        agent()
            .get(url(&self.url, path))
            .header("authorization", &format!("Bearer {}", self.token))
            .call()
            .with_context(|| format!("reach {}", self.url))
    }

    fn post<T: Serialize>(&self, path: &str, body: &T) -> Result<ureq::http::Response<ureq::Body>> {
        agent()
            .post(url(&self.url, path))
            .header("authorization", &format!("Bearer {}", self.token))
            .send_json(body)
            .with_context(|| format!("reach {}", self.url))
    }

    pub fn presence(&self, presence: &Presence) -> Result<Devices> {
        read(self.post("/api/presence", presence)?)
    }

    pub fn devices(&self) -> Result<Devices> {
        read(self.get("/api/devices")?)
    }

    pub fn relay(&self, device_id: &str) -> Result<Relay> {
        read(self.post(
            "/api/relay",
            &RelayRequest {
                device_id: device_id.to_string(),
            },
        )?)
    }

    pub fn wake(&self, device_id: &str) -> Result<()> {
        let r = self.post(
            "/api/wake",
            &WakeRequest {
                device_id: device_id.to_string(),
            },
        )?;
        if r.status().is_success() {
            return Ok(());
        }
        read::<serde_json::Value>(r).map(|_| ())
    }

    pub fn remove(&self, device_id: &str) -> Result<()> {
        let r = agent()
            .delete(url(&self.url, &format!("/api/devices/{device_id}")))
            .header("authorization", &format!("Bearer {}", self.token))
            .call()?;
        if r.status().is_success() {
            return Ok(());
        }
        read::<serde_json::Value>(r).map(|_| ())
    }
}

/// The account's devices as last fetched (empty without a login).
pub fn cached_devices(paths: &Paths) -> Vec<Device> {
    std::fs::read_to_string(paths.hub_devices())
        .ok()
        .and_then(|s| serde_json::from_str::<Devices>(&s).ok())
        .map(|d| d.devices)
        .unwrap_or_default()
}

/// Remember the device list, and add the account's hosts to the known
/// hosts so they show up and connect like paired ones.
pub fn store_devices(paths: &Paths, devices: &Devices) -> Result<()> {
    std::fs::write(paths.hub_devices(), serde_json::to_string(devices)?)?;
    let mut known = TrustStore::load(&paths.known_hosts())?;
    let before = known.peers.clone();
    for d in &devices.devices {
        let (Some(host), false) = (&d.host, d.this_device) else {
            continue;
        };
        let Some(fp) = fingerprint(&d.fingerprint) else {
            continue;
        };
        let address = host.addresses.first().map(|a| join(a, host.port));
        match known.peers.iter_mut().find(|p| p.fingerprint == fp) {
            // Paired before: keep its name; refresh the address only if it
            // had none.
            Some(p) => {
                if p.address.is_none() {
                    p.address = address;
                }
            }
            None => {
                known.add_distinct(TrustedPeer {
                    name: d.name.clone(),
                    fingerprint: fp,
                    address,
                });
            }
        }
    }
    if known.peers != before {
        known.save(&paths.known_hosts())?;
    }
    Ok(())
}

pub fn fingerprint(hex_fp: &str) -> Option<Fingerprint> {
    Some(Fingerprint(hex::decode(hex_fp).ok()?.try_into().ok()?))
}

/// "ip:port", bracketing IPv6.
pub fn join(ip: &str, port: u16) -> String {
    if ip.contains(':') {
        format!("[{ip}]:{port}")
    } else {
        format!("{ip}:{port}")
    }
}

/// This machine's addresses for others to reach it: LAN IPv4 first, no
/// loopback, link-local or container bridges.
pub fn local_addresses() -> Vec<String> {
    let mut v4 = Vec::new();
    let mut v6 = Vec::new();
    for iface in if_addrs::get_if_addrs().unwrap_or_default() {
        let name = iface.name.to_ascii_lowercase();
        if iface.is_loopback()
            || ["docker", "br-", "veth", "virbr", "vethernet"]
                .iter()
                .any(|p| name.starts_with(p))
        {
            continue;
        }
        match iface.ip() {
            std::net::IpAddr::V4(ip) if !ip.is_link_local() => v4.push(ip),
            std::net::IpAddr::V6(ip) if ip.segments()[0] & 0xffc0 != 0xfe80 => v6.push(ip),
            _ => {}
        }
    }
    // Private (LAN) addresses before others such as VPN overlays.
    v4.sort_by_key(|ip| !ip.is_private());
    v4.iter()
        .map(|ip| ip.to_string())
        .chain(v6.iter().map(|ip| ip.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_from_the_hub_join_the_known_hosts() {
        let dir = std::env::temp_dir().join(format!("oriel-hub-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let paths = Paths {
            config_dir: dir.clone(),
        };
        let mut known = TrustStore::default();
        known.upsert(TrustedPeer {
            name: "desk".into(),
            fingerprint: Fingerprint([1; 32]),
            address: Some("10.0.0.1:48600".into()),
        });
        known.save(&paths.known_hosts()).unwrap();
        let device = |name: &str, fp: u8, host: bool| Device {
            id: name.into(),
            name: name.into(),
            fingerprint: hex::encode([fp; 32]),
            platform: "windows".into(),
            host: host.then(|| HostInfo {
                port: 48600,
                addresses: vec!["192.168.1.5".into()],
            }),
            online: true,
            last_seen: 0,
            this_device: false,
            wakeable: false,
        };
        let devices = Devices {
            devices: vec![
                device("watermelon", 1, true),
                device("tangelo", 2, true),
                device("phone", 3, false),
            ],
            user: "me".into(),
        };
        store_devices(&paths, &devices).unwrap();
        let known = TrustStore::load(&paths.known_hosts()).unwrap();
        // The paired host keeps its name and address; the new one is added.
        assert_eq!(known.peers.len(), 2);
        assert_eq!(
            known.lookup("desk").unwrap().address.as_deref(),
            Some("10.0.0.1:48600")
        );
        assert_eq!(
            known.lookup("tangelo").unwrap().address.as_deref(),
            Some("192.168.1.5:48600")
        );
        assert_eq!(cached_devices(&paths).len(), 3);
        let _ = std::fs::remove_dir_all(dir);
    }
}
