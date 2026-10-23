//! Connecting to a host: address resolution, fingerprint pinning, pairing.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use oriel_net::{
    ControlStream, Fingerprint, Identity, Paths, TrustStore, TrustedPeer, client_endpoint, pairing,
    peer_fingerprint,
};
use oriel_proto::{ClientMsg, DEFAULT_PORT, DisplayInfo, HostMsg, PROTOCOL_VERSION};

pub struct Connected {
    pub endpoint: quinn::Endpoint,
    pub conn: quinn::Connection,
    pub ctl: ControlStream,
    pub host_name: String,
    pub displays: Vec<DisplayInfo>,
    /// Codecs the host can encode.
    pub codecs: Vec<oriel_proto::Codec>,
    /// Host sound outputs the user can choose from.
    pub audio_devices: Vec<oriel_proto::AudioDevice>,
    /// The host can create virtual monitors.
    pub virtual_display: bool,
    /// The host's protocol version (another one than ours is worth a
    /// warning, not a refusal).
    pub host_protocol: u32,
}

/// Resolve `host[:port]` (IPv6 addresses with a port go in brackets).
pub async fn resolve(address: &str) -> Result<SocketAddr> {
    let with_port = if let Ok(addr) = address.parse::<SocketAddr>() {
        return Ok(addr);
    } else if let Ok(ip) = address.parse::<std::net::IpAddr>() {
        return Ok(SocketAddr::new(ip, DEFAULT_PORT));
    } else if address
        .rsplit_once(':')
        .is_some_and(|(_, port)| port.parse::<u16>().is_ok())
    {
        address.to_string()
    } else {
        format!("{address}:{DEFAULT_PORT}")
    };
    tokio::net::lookup_host(&with_port)
        .await
        .with_context(|| format!("resolve {with_port}"))?
        .next()
        .with_context(|| format!("{with_port} did not resolve"))
}

type Opened = (
    quinn::Endpoint,
    quinn::Connection,
    ControlStream,
    Fingerprint,
);

async fn open(identity: &Identity, address: &str, pair: bool) -> Result<Opened> {
    open_within(identity, address, pair, Duration::from_secs(8)).await
}

async fn open_within(
    identity: &Identity,
    address: &str,
    pair: bool,
    timeout: Duration,
) -> Result<Opened> {
    let addr = resolve(address).await?;
    let endpoint = client_endpoint(identity)?;
    handshake(endpoint, addr, pair, timeout).await
}

/// Through the hub's relay: introduce this socket to the relay, then run
/// QUIC over it (still end to end with the host).
async fn open_relayed(identity: &Identity, relay: &oriel_proto::hub::Relay) -> Result<Opened> {
    use oriel_proto::hub::{RELAY_HELLO, RELAY_READY};
    let addr = resolve(&relay.address).await?;
    let mut hello = RELAY_HELLO.to_vec();
    hello.extend(hex::decode(&relay.token).context("relay token")?);
    let socket = tokio::task::spawn_blocking(move || -> Result<std::net::UdpSocket> {
        let socket = std::net::UdpSocket::bind(if addr.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        })?;
        socket.set_read_timeout(Some(Duration::from_millis(400)))?;
        let mut buf = [0u8; 64];
        for _ in 0..10 {
            socket.send_to(&hello, addr)?;
            if let Ok((n, from)) = socket.recv_from(&mut buf)
                && from == addr
                && &buf[..n] == RELAY_READY
            {
                return Ok(socket);
            }
        }
        anyhow::bail!("the hub's relay at {addr} does not answer")
    })
    .await??;
    let endpoint = oriel_net::client_endpoint_on(identity, socket)?;
    handshake(endpoint, addr, false, Duration::from_secs(8)).await
}

async fn handshake(
    endpoint: quinn::Endpoint,
    addr: SocketAddr,
    pair: bool,
    timeout: Duration,
) -> Result<Opened> {
    let conn = tokio::time::timeout(timeout, endpoint.connect(addr, "oriel")?)
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "no answer from {addr}. Check that oriel is sharing there and that its \
                 firewall lets UDP port {} in",
                addr.port()
            )
        })?
        .with_context(|| format!("connect to {addr}"))?;
    let host_fp = peer_fingerprint(&conn)?;
    let (send, recv) = conn.open_bi().await?;
    let mut ctl = ControlStream { send, recv };
    ctl.send(&ClientMsg::Hello {
        protocol: PROTOCOL_VERSION,
        name: device_name(),
        pair,
    })
    .await?;
    Ok((endpoint, conn, ctl, host_fp))
}

/// Reach a host: its known address; with a hub login, also the other
/// addresses it reported (all at once), and finally the hub's relay.
/// Returns the address that answered (`None` when relayed).
async fn open_any(
    paths: &Paths,
    identity: Arc<Identity>,
    entry: Option<&oriel_net::TrustedPeer>,
    address: &str,
) -> Result<(
    quinn::Endpoint,
    quinn::Connection,
    ControlStream,
    Fingerprint,
    Option<String>,
)> {
    let account = oriel_net::hub::account(paths);
    if account.is_some() {
        refresh_hub(paths).await;
    }
    let device = entry.zip(account.as_ref()).and_then(|(e, _)| {
        oriel_net::hub::cached_devices(paths)
            .into_iter()
            .find(|d| oriel_net::hub::fingerprint(&d.fingerprint) == Some(e.fingerprint))
    });
    let mut candidates = vec![address.to_string()];
    if let Some(host) = device.as_ref().and_then(|d| d.host.as_ref()) {
        for a in &host.addresses {
            let a = oriel_net::hub::join(a, host.port);
            if !candidates.contains(&a) {
                candidates.push(a);
            }
        }
    }
    let Some(device) = device else {
        let (e, c, ctl, fp) = open(&identity, address, false).await?;
        return Ok((e, c, ctl, fp, Some(address.to_string())));
    };
    // Every direct address at once; the first to answer wins.
    // Debugging aid: ORIEL_DEBUG_FORCE_RELAY=1 goes through the hub.
    let force_relay = std::env::var_os("ORIEL_DEBUG_FORCE_RELAY").is_some();
    let mut attempts = tokio::task::JoinSet::new();
    for candidate in candidates.clone().into_iter().filter(|_| !force_relay) {
        let identity = identity.clone();
        attempts.spawn(async move {
            open_within(&identity, &candidate, false, Duration::from_secs(4))
                .await
                .map(|o| (o, candidate))
        });
    }
    let mut first_error = None;
    while let Some(result) = attempts.join_next().await {
        match result {
            Ok(Ok(((e, c, ctl, fp), candidate))) => {
                attempts.abort_all();
                return Ok((e, c, ctl, fp, Some(candidate)));
            }
            Ok(Err(e)) => {
                first_error.get_or_insert(e);
            }
            Err(_) => {}
        }
    }
    // Not reachable directly (away from home): through the hub.
    let account = account.expect("checked above");
    tracing::info!(host = %device.name, "no direct route; asking the hub to relay");
    let id = device.id.clone();
    let relay = tokio::task::spawn_blocking(move || account.relay(&id))
        .await?
        .map_err(|e| {
            let direct = first_error.map(|d| format!("{d:#}")).unwrap_or_default();
            anyhow::anyhow!("{direct}; the hub cannot relay either: {e:#}")
        })?;
    let (e, c, ctl, fp) = open_relayed(&identity, &relay).await?;
    Ok((e, c, ctl, fp, None))
}

/// Fetch the hub account's devices (a few seconds at most), adding its
/// hosts to the known hosts.
async fn refresh_hub(paths: &Paths) {
    let p = paths.clone();
    let fetch = tokio::task::spawn_blocking(move || -> Result<()> {
        if let Some(account) = oriel_net::hub::account(&p) {
            oriel_net::hub::store_devices(&p, &account.devices()?)?;
        }
        Ok(())
    });
    match tokio::time::timeout(Duration::from_secs(4), fetch).await {
        Ok(Ok(Err(e))) => tracing::warn!("cannot reach the hub: {e:#}"),
        Err(_) => tracing::warn!("the hub does not answer"),
        _ => {}
    }
}

/// Connect to a paired host. `target` is a known host's name or an address.
pub async fn connect(paths: &Paths, target: &str) -> Result<Connected> {
    let identity = Identity::load_or_create(&paths.config_dir)?;
    let mut known = TrustStore::load(&paths.known_hosts())?;
    // A host of the hub account not seen yet.
    if known.lookup(target).is_none() && oriel_net::hub::account(paths).is_some() {
        refresh_hub(paths).await;
        known = TrustStore::load(&paths.known_hosts())?;
    }
    let entry = known.lookup(target).cloned();
    let address = entry
        .as_ref()
        .and_then(|e| {
            (e.name.eq_ignore_ascii_case(target))
                .then(|| e.address.clone())
                .flatten()
        })
        .unwrap_or_else(|| target.to_string());
    let (endpoint, conn, mut ctl, host_fp, reached) =
        open_any(&paths.clone(), Arc::new(identity), entry.as_ref(), &address).await?;
    // Check the host before reading anything it says.
    let Some(trusted) = known.by_fingerprint(&host_fp).cloned() else {
        conn.close(0u32.into(), b"unknown host");
        if entry.is_some() {
            bail!(
                "the host at {address} presented a different identity ({}) than when it was paired. \
                 If it was reinstalled, pair again with `oriel pair`",
                host_fp.short()
            );
        }
        bail!("{address} is not a paired host; pair first with `oriel pair {address} --pin <PIN>`");
    };
    let welcome = ctl.recv().await.map_err(|e| {
        if e.downcast_ref::<oriel_proto::ProtoError>().is_some() {
            anyhow::anyhow!(
                "cannot understand the host: it runs a version of oriel too different from \
                 this one (protocol {PROTOCOL_VERSION}); update both"
            )
        } else {
            e
        }
    })?;
    match welcome {
        HostMsg::Welcome {
            protocol,
            name,
            displays,
            codecs,
            audio_devices,
            virtual_display,
        } => {
            if protocol != PROTOCOL_VERSION {
                tracing::warn!(
                    host = protocol,
                    client = PROTOCOL_VERSION,
                    "the host speaks another protocol version"
                );
            }
            let address = reached.unwrap_or_else(|| trusted.address.clone().unwrap_or_default());
            if trusted.address.as_deref() != Some(address.as_str()) || trusted.name != name {
                let mut updated = trusted.clone();
                updated.address = Some(address.clone());
                known.upsert(updated);
                known.save(&paths.known_hosts())?;
            }
            Ok(Connected {
                endpoint,
                conn,
                ctl,
                host_name: name,
                displays,
                codecs,
                audio_devices,
                virtual_display,
                host_protocol: protocol,
            })
        }
        HostMsg::Rejected { reason } => bail!("host refused the connection: {reason}"),
        other => bail!("unexpected reply from host: {other:?}"),
    }
}

/// Whether a paired host answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    Online,
    Offline,
    /// Something answers at the address, but with another identity.
    Changed,
}

/// Check every known host with a bare handshake (no session, nothing shown
/// on the host). Results come in the order of `hosts`.
pub async fn probe(paths: &Paths, hosts: &[TrustedPeer]) -> Result<Vec<Reach>> {
    let identity = Identity::load_or_create(&paths.config_dir)?;
    let endpoint = client_endpoint(&identity)?;
    let checks: Vec<_> = hosts
        .iter()
        .cloned()
        .map(|host| {
            let endpoint = endpoint.clone();
            tokio::spawn(async move {
                let Some(address) = &host.address else {
                    return Reach::Offline;
                };
                let attempt = async {
                    let addr = resolve(address).await?;
                    let conn = endpoint.connect(addr, "oriel")?.await?;
                    let fingerprint = peer_fingerprint(&conn)?;
                    conn.close(0u32.into(), PROBE_REASON);
                    anyhow::Ok(fingerprint)
                };
                match tokio::time::timeout(Duration::from_secs(3), attempt).await {
                    Ok(Ok(fp)) if fp == host.fingerprint => Reach::Online,
                    Ok(Ok(_)) => Reach::Changed,
                    _ => Reach::Offline,
                }
            })
        })
        .collect();
    let mut results = Vec::with_capacity(checks.len());
    for check in checks {
        results.push(check.await.unwrap_or(Reach::Offline));
    }
    endpoint.wait_idle().await;
    Ok(results)
}

/// The close reason of a reachability check, which hosts don't log.
pub const PROBE_REASON: &[u8] = oriel_proto::PROBE_REASON;

/// Pair with the host at `address` using the PIN it shows. Returns its name.
pub async fn pair(paths: &Paths, address: &str, pin: &str, name: Option<&str>) -> Result<String> {
    let identity = Identity::load_or_create(&paths.config_dir)?;
    let (_endpoint, conn, mut ctl, host_fp) = open(&identity, address, true).await?;
    let host_name = match ctl.recv().await? {
        HostMsg::Welcome { name, .. } => name,
        HostMsg::Rejected { reason } => bail!("host refused pairing: {reason}"),
        other => bail!("unexpected reply from host: {other:?}"),
    };
    pairing::pair_as_client(&mut ctl, pin, &identity.fingerprint, &host_fp).await?;
    let name = name.map(str::to_string).unwrap_or(host_name);
    let mut known = TrustStore::load(&paths.known_hosts())?;
    known.upsert(TrustedPeer {
        name: name.clone(),
        fingerprint: host_fp,
        address: Some(address.to_string()),
    });
    known.save(&paths.known_hosts())?;
    conn.close(0u32.into(), b"paired");
    Ok(name)
}

/// What the platform calls this device, where the environment does not
/// say: macOS has no /etc/hostname (and shells do not export HOSTNAME), and
/// Android has neither.
fn platform_name() -> Option<String> {
    #[cfg(target_os = "macos")]
    return std::process::Command::new("scutil")
        .args(["--get", "ComputerName"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());
    // The name shown in Settings → About phone, else the model.
    #[cfg(target_os = "android")]
    return crate::android::device_name().or_else(|| {
        android_system_properties::AndroidSystemProperties::new()
            .get("ro.product.model")
            .filter(|s| !s.is_empty())
    });
    #[allow(unreachable_code)]
    None
}

pub fn device_name() -> String {
    std::env::var("ORIEL_NAME")
        .ok()
        .or_else(platform_name)
        .or_else(|| std::env::var("HOSTNAME").ok())
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_string())
        })
        .filter(|s| !s.is_empty())
        .or_else(system_hostname)
        .unwrap_or_else(|| "oriel-client".into())
}

/// The kernel's hostname (Linux without /etc/hostname, macOS without a
/// computer name).
fn system_hostname() -> Option<String> {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        // SAFETY: gethostname writes at most buf.len() bytes.
        let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0;
        let len = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
        let name = String::from_utf8_lossy(&buf[..len]).trim().to_string();
        // Android reports "localhost".
        return (ok && !name.is_empty() && name != "localhost").then_some(name);
    }
    #[allow(unreachable_code)]
    None
}
