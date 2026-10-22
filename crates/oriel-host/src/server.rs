//! Accepts connections, authenticates clients and runs pairing and sessions.

use std::{net::SocketAddr, sync::Arc, time::Duration};

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
    /// The connection currently streaming, replaced by newer sessions.
    active: Mutex<Option<quinn::Connection>>,
}

struct PairingState {
    pin: Option<String>,
    failures: u32,
}

/// What the host is doing, for a tray icon or status display.
#[derive(Debug, Clone, Default)]
pub struct HostStatus {
    /// The client currently streaming.
    pub client: Option<String>,
    pub codec: Option<String>,
}

/// Lets a UI watch a running host and allow pairing.
#[derive(Clone)]
pub struct HostControl {
    pairing: Arc<Mutex<PairingState>>,
    status: Arc<Mutex<HostStatus>>,
}

impl HostControl {
    pub fn new(pin: Option<String>) -> Self {
        Self {
            pairing: Arc::new(Mutex::new(PairingState { pin, failures: 0 })),
            status: Default::default(),
        }
    }

    pub fn status(&self) -> HostStatus {
        self.status.lock().clone()
    }

    /// Accept pairing with a fresh PIN, which is returned.
    pub fn allow_pairing(&self) -> String {
        let pin = oriel_net::pairing::generate_pin();
        let mut state = self.pairing.lock();
        state.pin = Some(pin.clone());
        state.failures = 0;
        pin
    }

    pub fn pairing_pin(&self) -> Option<String> {
        self.pairing.lock().pin.clone()
    }
}

const MAX_PAIR_FAILURES: u32 = 5;
/// A PIN in this file (in the config directory) allows pairing for
/// [`PIN_FILE_VALIDITY`] after it was written; used by unattended hosts
/// (`oriel service pair`).
pub const PIN_FILE: &str = "pairing-pin";
const PIN_FILE_VALIDITY: Duration = Duration::from_secs(600);

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
    let host = Arc::new(Host {
        pairing: control.pairing.clone(),
        status: control.status.clone(),
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
        let (send, recv) = tokio::time::timeout(Duration::from_secs(10), conn.accept_bi())
            .await
            .context("client did not open a control stream")??;
        let mut ctl = ControlStream { send, recv };
        let ClientMsg::Hello {
            protocol,
            name,
            pair,
        } = ctl.recv().await?
        else {
            anyhow::bail!("expected Hello");
        };
        if protocol != PROTOCOL_VERSION {
            return reject(
                &conn,
                &mut ctl,
                format!(
                    "protocol version {protocol} is not supported (host speaks {PROTOCOL_VERSION})"
                ),
            )
            .await;
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
        })
        .await?;

        // One streaming session at a time: the newest wins.
        if let Some(previous) = self.active.lock().replace(conn.clone()) {
            previous.close(1u32.into(), b"another session took over");
        }
        self.status.lock().client = Some(name.clone());
        let result = crate::session::run(
            conn.clone(),
            ctl,
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
            *self.status.lock() = HostStatus::default();
        }
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
        let pin_file = self.paths.config_dir.join(PIN_FILE);
        let file_pin = std::fs::metadata(&pin_file)
            .ok()
            .and_then(|m| m.modified().ok())
            .filter(|t| t.elapsed().is_ok_and(|age| age < PIN_FILE_VALIDITY))
            .and_then(|_| std::fs::read_to_string(&pin_file).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let pin = {
            let state = self.pairing.lock();
            state.pin.clone().or(file_pin.clone())
        };
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
        })
        .await?;
        if !pairing::pair_as_host(&mut ctl, &pin, &client_fp, &self.identity.fingerprint).await? {
            {
                let mut state = self.pairing.lock();
                state.failures += 1;
                tracing::warn!(client = %name, failures = state.failures, "pairing attempt with wrong PIN");
                if state.failures >= MAX_PAIR_FAILURES {
                    state.pin = None;
                    tracing::warn!(
                        "too many failed pairing attempts; pairing disabled until restart"
                    );
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
        if file_pin.is_some() {
            let _ = std::fs::remove_file(&pin_file);
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
