//! Accepts connections, authenticates clients and runs pairing and sessions.

use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use oriel_net::{
    ControlStream, Fingerprint, Identity, Paths, TrustStore, TrustedPeer, pairing,
    peer_fingerprint, server_endpoint,
};
use oriel_proto::{ClientMsg, Codec, HostMsg, PROTOCOL_VERSION};
use parking_lot::Mutex;

use crate::capture::{self, SourceKind, TestState};

#[derive(Debug, Clone)]
pub struct HostOptions {
    pub listen: SocketAddr,
    pub name: String,
    pub source: SourceKind,
    /// Accept pairing requests with this PIN.
    pub pair_pin: Option<String>,
    /// Force an FFmpeg encoder (e.g. `hevc_nvenc`).
    pub encoder: Option<String>,
    /// Restrict to this codec.
    pub codec: Option<Codec>,
    pub default_bitrate_kbps: u32,
}

struct Host {
    options: Arc<HostOptions>,
    paths: Paths,
    identity: Identity,
    test: TestState,
    pairing: Arc<Mutex<PairingState>>,
    status: Arc<Mutex<HostStatus>>,
    sessions: Arc<Mutex<Vec<quinn::Connection>>>,
    /// The connection currently streaming, replaced by newer sessions.
    active: Mutex<Option<quinn::Connection>>,
}

struct PairingState {
    pin: Option<String>,
    /// When `pin` was set; it is valid for [`PIN_VALIDITY`].
    granted: Instant,
    failures: u32,
}

impl PairingState {
    fn new(pin: Option<String>) -> Self {
        Self {
            pin,
            granted: Instant::now(),
            failures: 0,
        }
    }

    /// The PIN, unless it has expired.
    fn current(&mut self) -> Option<String> {
        if self.granted.elapsed() >= PIN_VALIDITY {
            self.pin = None;
        }
        self.pin.clone()
    }
}

/// A client with an open session.
#[derive(Debug, Clone)]
pub struct ClientInfo {
    /// Identifies the session for [`HostControl::kick`].
    pub id: usize,
    pub name: String,
    pub address: SocketAddr,
    pub codec: Option<String>,
}

/// What the host is doing, for a tray icon or status display.
#[derive(Debug, Clone, Default)]
pub struct HostStatus {
    pub clients: Vec<ClientInfo>,
}

impl HostStatus {
    pub(crate) fn set_codec(&mut self, id: usize, codec: &str) {
        if let Some(c) = self.clients.iter_mut().find(|c| c.id == id) {
            c.codec = Some(codec.to_string());
        }
    }
}

/// Lets a UI watch a running host and allow pairing.
#[derive(Clone)]
pub struct HostControl {
    pairing: Arc<Mutex<PairingState>>,
    status: Arc<Mutex<HostStatus>>,
    sessions: Arc<Mutex<Vec<quinn::Connection>>>,
}

impl HostControl {
    pub fn new(pin: Option<String>) -> Self {
        Self {
            pairing: Arc::new(Mutex::new(PairingState::new(pin))),
            status: Default::default(),
            sessions: Default::default(),
        }
    }

    /// End a client's session.
    pub fn kick(&self, id: usize) {
        if let Some(conn) = self.sessions.lock().iter().find(|c| c.stable_id() == id) {
            tracing::info!(remote = %conn.remote_address(), "disconnecting client");
            conn.close(3u32.into(), b"disconnected by the host");
        }
    }

    /// Stop accepting pairing requests.
    pub fn stop_pairing(&self) {
        self.pairing.lock().pin = None;
    }

    pub fn status(&self) -> HostStatus {
        self.status.lock().clone()
    }

    /// Accept pairing with a fresh PIN, which is returned.
    pub fn allow_pairing(&self) -> String {
        let pin = oriel_net::pairing::generate_pin();
        *self.pairing.lock() = PairingState::new(Some(pin.clone()));
        pin
    }

    pub fn pairing_pin(&self) -> Option<String> {
        self.pairing.lock().current()
    }
}

const MAX_PAIR_FAILURES: u32 = 5;
/// A PIN in this file under the config directory allows pairing for
/// [`PIN_VALIDITY`] after it was written; used by unattended hosts
/// (`oriel service pair`).
pub fn pin_file(config_dir: &std::path::Path) -> std::path::PathBuf {
    config_dir.join("pairing").join("pin")
}
/// The service's open sessions, one per line: id, name, address, codec
/// (tab separated), for the signed-in user's tray icon.
pub fn sessions_file(config_dir: &std::path::Path) -> std::path::PathBuf {
    config_dir.join("pairing").join("sessions")
}

/// Session ids to disconnect, one per line, written by the tray icon.
pub fn kick_file(config_dir: &std::path::Path) -> std::path::PathBuf {
    config_dir.join("pairing").join("kick")
}

impl ClientInfo {
    fn to_line(&self) -> String {
        let clean = |s: &str| s.replace(['\t', '\n', '\r'], " ");
        format!(
            "{}\t{}\t{}\t{}",
            self.id,
            clean(&self.name),
            self.address,
            clean(self.codec.as_deref().unwrap_or(""))
        )
    }

    pub fn parse_line(line: &str) -> Option<Self> {
        let mut parts = line.split('\t');
        Some(Self {
            id: parts.next()?.parse().ok()?,
            name: parts.next()?.to_string(),
            address: parts.next()?.parse().ok()?,
            codec: parts.next().filter(|c| !c.is_empty()).map(str::to_string),
        })
    }
}

/// Where the pairing directory exists (the service's), publish the open
/// sessions there and act on disconnect requests, for the tray icon of a
/// user who cannot talk to the service otherwise.
fn publish_sessions(config_dir: std::path::PathBuf, control: HostControl) {
    if !config_dir.join("pairing").is_dir() {
        return;
    }
    tokio::spawn(async move {
        let sessions = sessions_file(&config_dir);
        let kick = kick_file(&config_dir);
        let _ = std::fs::remove_file(&kick);
        let mut written: Option<String> = None;
        loop {
            let text: String = control
                .status()
                .clients
                .iter()
                .map(|c| c.to_line() + "\n")
                .collect();
            if written.as_ref() != Some(&text) {
                if let Err(e) = std::fs::write(&sessions, &text) {
                    tracing::debug!("cannot publish sessions: {e:#}");
                }
                written = Some(text);
            }
            if let Ok(ids) = std::fs::read_to_string(&kick) {
                let _ = std::fs::remove_file(&kick);
                for id in ids.lines().filter_map(|l| l.trim().parse().ok()) {
                    control.kick(id);
                }
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });
}

/// A PIN pairs one device, within this time.
const PIN_VALIDITY: Duration = Duration::from_secs(600);

pub async fn run(paths: Paths, options: HostOptions) -> Result<()> {
    let control = HostControl::new(options.pair_pin.clone());
    run_with(paths, options, control).await
}

/// Run the host, observable and controllable through `control`.
pub async fn run_with(paths: Paths, options: HostOptions, control: HostControl) -> Result<()> {
    crate::init_process();
    let identity = Identity::load_or_create(&paths.config_dir)?;
    // A restarting host may still hold the port for a moment.
    let mut attempt = 0;
    let endpoint = loop {
        match server_endpoint(&identity, options.listen) {
            Ok(e) => break e,
            Err(e) if attempt < 20 => {
                attempt += 1;
                tracing::debug!("cannot listen yet: {e:#}");
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            Err(e) => return Err(e),
        }
    };
    tracing::info!(
        listen = %endpoint.local_addr()?,
        fingerprint = %identity.fingerprint.short(),
        name = %options.name,
        "host ready"
    );
    if let Some(pin) = &options.pair_pin {
        println!(
            "Pairing enabled. On the client run:\n  oriel pair <this-host-address> --pin {pin}"
        );
    }
    publish_sessions(paths.config_dir.clone(), control.clone());
    let host = Arc::new(Host {
        pairing: control.pairing.clone(),
        status: control.status.clone(),
        sessions: control.sessions.clone(),
        options: Arc::new(options),
        paths,
        identity,
        test: TestState::default(),
        active: Mutex::new(None),
    });

    while let Some(incoming) = endpoint.accept().await {
        let host = host.clone();
        tokio::spawn(async move {
            let remote = incoming.remote_address();
            match incoming.await {
                Ok(conn) => {
                    if let Err(e) = host.handle(conn).await {
                        tracing::info!(%remote, "connection ended: {e:#}");
                    }
                }
                Err(e) => tracing::debug!(%remote, "handshake failed: {e}"),
            }
        });
    }
    Ok(())
}

impl Host {
    async fn handle(self: Arc<Self>, conn: quinn::Connection) -> Result<()> {
        let remote = conn.remote_address();
        let client_fp = peer_fingerprint(&conn)?;
        let (send, recv) = match tokio::time::timeout(Duration::from_secs(10), conn.accept_bi())
            .await
            .context("client did not open a control stream")?
        {
            Ok(streams) => streams,
            // A client checking whether this host is up.
            Err(quinn::ConnectionError::ApplicationClosed(close))
                if close.reason.as_ref() == oriel_proto::PROBE_REASON =>
            {
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };
        let mut ctl = ControlStream { send, recv };
        let ClientMsg::Hello {
            protocol,
            name,
            pair,
        } = ctl.recv().await?
        else {
            anyhow::bail!("expected Hello");
        };
        // Another version may still work for what both understand; the
        // client warns, and the session refuses settings it cannot read.
        if protocol != PROTOCOL_VERSION {
            tracing::warn!(%remote, client = protocol, host = PROTOCOL_VERSION, "client speaks another protocol version");
        }
        tracing::info!(%remote, client = %name, fingerprint = %client_fp.short(), pair, "client connected");

        if pair {
            return self.pair(conn, ctl, name, client_fp).await;
        }

        let trusted = TrustStore::load(&self.paths.trusted_clients())?;
        if trusted.by_fingerprint(&client_fp).is_none() {
            return reject(
                &conn,
                &mut ctl,
                "this client is not paired with the host".into(),
            )
            .await;
        }
        ctl.send(&HostMsg::Welcome {
            protocol: PROTOCOL_VERSION,
            name: self.options.name.clone(),
            displays: capture::list_displays(self.options.source),
            codecs: oriel_media::available_codecs(),
            audio_devices: match self.options.source {
                capture::SourceKind::Auto => oriel_audio::output_devices()
                    .into_iter()
                    .map(|d| oriel_proto::AudioDevice {
                        id: d.id,
                        name: d.name,
                    })
                    .collect(),
                _ => Vec::new(),
            },
            virtual_display: capture::virtual_display_available(self.options.source),
        })
        .await?;

        // One streaming session at a time: the newest wins.
        if let Some(previous) = self.active.lock().replace(conn.clone()) {
            previous.close(1u32.into(), b"another session took over");
        }
        let id = conn.stable_id();
        self.sessions.lock().push(conn.clone());
        self.status.lock().clients.push(ClientInfo {
            id,
            name: name.clone(),
            address: remote,
            codec: None,
        });
        let result = crate::session::run(
            conn.clone(),
            ctl,
            protocol,
            self.options.clone(),
            self.test.clone(),
            self.status.clone(),
        )
        .await;
        let mut active = self.active.lock();
        if active
            .as_ref()
            .is_some_and(|c| c.stable_id() == conn.stable_id())
        {
            *active = None;
        }
        drop(active);
        self.sessions.lock().retain(|c| c.stable_id() != id);
        self.status.lock().clients.retain(|c| c.id != id);
        conn.close(0u32.into(), b"bye");
        result
    }

    async fn pair(
        &self,
        conn: quinn::Connection,
        mut ctl: ControlStream,
        name: String,
        client_fp: Fingerprint,
    ) -> Result<()> {
        let pin_file = pin_file(&self.paths.config_dir);
        let file_pin = std::fs::metadata(&pin_file)
            .ok()
            .and_then(|m| m.modified().ok())
            .filter(|t| t.elapsed().is_ok_and(|age| age < PIN_VALIDITY))
            .and_then(|_| std::fs::read_to_string(&pin_file).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let pin = { self.pairing.lock().current().or(file_pin.clone()) };
        let Some(pin) = pin else {
            return reject(
                &conn,
                &mut ctl,
                "the host is not accepting pairing requests (start it with --pair)".into(),
            )
            .await;
        };
        ctl.send(&HostMsg::Welcome {
            protocol: PROTOCOL_VERSION,
            name: self.options.name.clone(),
            displays: Vec::new(),
            codecs: Vec::new(),
            audio_devices: Vec::new(),
            virtual_display: false,
        })
        .await?;
        if !pairing::pair_as_host(&mut ctl, &pin, &client_fp, &self.identity.fingerprint).await? {
            {
                let mut state = self.pairing.lock();
                state.failures += 1;
                tracing::warn!(client = %name, failures = state.failures, "pairing attempt with wrong PIN");
                if state.failures >= MAX_PAIR_FAILURES {
                    state.pin = None;
                    tracing::warn!("too many failed pairing attempts; pairing needs a new PIN");
                }
            }
            finish(&conn, &mut ctl).await;
            return Ok(());
        }
        let mut trusted = TrustStore::load(&self.paths.trusted_clients())?;
        trusted.upsert(TrustedPeer {
            name: name.clone(),
            fingerprint: client_fp,
            address: None,
        });
        trusted.save(&self.paths.trusted_clients())?;
        // Each PIN pairs one device.
        if file_pin.as_deref() == Some(pin.as_str()) {
            let _ = std::fs::remove_file(&pin_file);
        } else {
            self.pairing.lock().pin = None;
        }
        ctl.send(&HostMsg::PairResult {
            ok: true,
            message: format!("paired with {}", self.options.name),
        })
        .await?;
        finish(&conn, &mut ctl).await;
        tracing::info!(client = %name, fingerprint = %client_fp.short(), "client paired");
        Ok(())
    }
}

async fn reject(conn: &quinn::Connection, ctl: &mut ControlStream, reason: String) -> Result<()> {
    tracing::info!(remote = %conn.remote_address(), %reason, "rejecting client");
    ctl.send(&HostMsg::Rejected { reason }).await?;
    finish(conn, ctl).await;
    Ok(())
}

/// Let the last control message reach the client before closing.
async fn finish(conn: &quinn::Connection, ctl: &mut ControlStream) {
    let _ = ctl.send.finish();
    let _ = tokio::time::timeout(Duration::from_secs(2), ctl.send.stopped()).await;
    conn.close(0u32.into(), b"done");
}

#[cfg(test)]
mod tests {
    use super::ClientInfo;

    #[test]
    fn session_lines_roundtrip() {
        let c = ClientInfo {
            id: 7,
            name: "tan\tgelo".into(),
            address: "192.168.6.5:4433".parse().unwrap(),
            codec: Some("HEVC".into()),
        };
        let back = ClientInfo::parse_line(&c.to_line()).unwrap();
        assert_eq!((back.id, back.name.as_str()), (7, "tan gelo"));
        assert_eq!(back.address, c.address);
        assert_eq!(back.codec.as_deref(), Some("HEVC"));
        let none = ClientInfo { codec: None, ..c };
        assert_eq!(ClientInfo::parse_line(&none.to_line()).unwrap().codec, None);
    }
}
