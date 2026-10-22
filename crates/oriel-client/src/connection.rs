//! Connecting to a host: address resolution, fingerprint pinning, pairing.

use std::{net::SocketAddr, time::Duration};

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

async fn open(
    identity: &Identity,
    address: &str,
    pair: bool,
) -> Result<(
    quinn::Endpoint,
    quinn::Connection,
    ControlStream,
    Fingerprint,
)> {
    let addr = resolve(address).await?;
    let endpoint = client_endpoint(identity)?;
    let conn = tokio::time::timeout(Duration::from_secs(8), endpoint.connect(addr, "oriel")?)
        .await
        .with_context(|| format!("timed out connecting to {addr}"))?
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

/// Connect to a paired host. `target` is a known host's name or an address.
pub async fn connect(paths: &Paths, target: &str) -> Result<Connected> {
    let identity = Identity::load_or_create(&paths.config_dir)?;
    let mut known = TrustStore::load(&paths.known_hosts())?;
    let entry = known.lookup(target).cloned();
    let address = entry
        .as_ref()
        .and_then(|e| {
            (e.name.eq_ignore_ascii_case(target))
                .then(|| e.address.clone())
                .flatten()
        })
        .unwrap_or_else(|| target.to_string());
    let (endpoint, conn, mut ctl, host_fp) = open(&identity, &address, false).await?;
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
    match ctl.recv().await? {
        HostMsg::Welcome {
            protocol,
            name,
            displays,
        } => {
            if protocol != PROTOCOL_VERSION {
                bail!("host speaks protocol {protocol}, this client {PROTOCOL_VERSION}");
            }
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
            })
        }
        HostMsg::Rejected { reason } => bail!("host refused the connection: {reason}"),
        other => bail!("unexpected reply from host: {other:?}"),
    }
}

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

pub fn device_name() -> String {
    std::env::var("ORIEL_NAME")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_string())
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "oriel-client".into())
}
