//! PIN pairing. The host shows a six-digit PIN; both sides run SPAKE2 with
//! it and then exchange HMACs over both certificate fingerprints, so a
//! successful exchange proves the PIN *and* that each side sees the other's
//! real certificate.

use anyhow::{Result, bail};
use hmac::{Hmac, KeyInit, Mac};
use oriel_proto::{ClientMsg, HostMsg, PairMsg};
use sha2::Sha256;
use spake2::{Ed25519Group, Identity as SpakeId, Password, Spake2};

use crate::{ControlStream, Fingerprint};

const ID_CLIENT: &[u8] = b"oriel-client";
const ID_HOST: &[u8] = b"oriel-host";

pub fn generate_pin() -> String {
    format!("{:06}", rand::random_range(0..1_000_000))
}

fn confirm_mac(key: &[u8], role: &[u8], client: &Fingerprint, host: &Fingerprint) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(b"oriel pair v1 ");
    mac.update(role);
    mac.update(&client.0);
    mac.update(&host.0);
    mac.finalize().into_bytes().to_vec()
}

fn verify_mac(
    key: &[u8],
    role: &[u8],
    client: &Fingerprint,
    host: &Fingerprint,
    tag: &[u8],
) -> bool {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(b"oriel pair v1 ");
    mac.update(role);
    mac.update(&client.0);
    mac.update(&host.0);
    mac.verify_slice(tag).is_ok()
}

/// Client side, after sending `Hello { pair: true }` and receiving `Welcome`.
pub async fn pair_as_client(
    ctl: &mut ControlStream,
    pin: &str,
    me: &Fingerprint,
    host: &Fingerprint,
) -> Result<()> {
    let (state, msg) = Spake2::<Ed25519Group>::start_a(
        &Password::new(pin.trim()),
        &SpakeId::new(ID_CLIENT),
        &SpakeId::new(ID_HOST),
    );
    ctl.send(&ClientMsg::Pair(PairMsg::Spake(msg))).await?;
    let HostMsg::Pair(PairMsg::Spake(theirs)) = ctl.recv().await? else {
        bail!("host did not continue pairing");
    };
    let key = state
        .finish(&theirs)
        .map_err(|e| anyhow::anyhow!("pairing failed: {e:?}"))?;
    ctl.send(&ClientMsg::Pair(PairMsg::Confirm(confirm_mac(
        &key, b"client", me, host,
    ))))
    .await?;
    match ctl.recv().await? {
        HostMsg::Pair(PairMsg::Confirm(tag)) if verify_mac(&key, b"host", me, host, &tag) => {}
        HostMsg::PairResult { message, .. } | HostMsg::Rejected { reason: message } => {
            bail!("pairing refused: {message}")
        }
        _ => bail!("pairing failed: wrong PIN or the connection is being intercepted"),
    }
    match ctl.recv().await? {
        HostMsg::PairResult { ok: true, .. } => Ok(()),
        HostMsg::PairResult { message, .. } => bail!("pairing refused: {message}"),
        _ => bail!("unexpected message during pairing"),
    }
}

/// Host side, after receiving `Hello { pair: true }` and sending `Welcome`.
/// Returns whether the client proved knowledge of `pin`.
pub async fn pair_as_host(
    ctl: &mut ControlStream,
    pin: &str,
    client: &Fingerprint,
    me: &Fingerprint,
) -> Result<bool> {
    let ClientMsg::Pair(PairMsg::Spake(theirs)) = ctl.recv().await? else {
        bail!("client did not start pairing");
    };
    let (state, msg) = Spake2::<Ed25519Group>::start_b(
        &Password::new(pin),
        &SpakeId::new(ID_CLIENT),
        &SpakeId::new(ID_HOST),
    );
    ctl.send(&HostMsg::Pair(PairMsg::Spake(msg))).await?;
    let key = state
        .finish(&theirs)
        .map_err(|e| anyhow::anyhow!("pairing failed: {e:?}"))?;
    let ClientMsg::Pair(PairMsg::Confirm(tag)) = ctl.recv().await? else {
        bail!("client did not confirm pairing");
    };
    if !verify_mac(&key, b"client", client, me, &tag) {
        ctl.send(&HostMsg::PairResult {
            ok: false,
            message: "wrong PIN".into(),
        })
        .await?;
        return Ok(false);
    }
    ctl.send(&HostMsg::Pair(PairMsg::Confirm(confirm_mac(
        &key, b"host", client, me,
    ))))
    .await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_binds_fingerprints() {
        let a = Fingerprint([1; 32]);
        let b = Fingerprint([2; 32]);
        let tag = confirm_mac(b"key", b"client", &a, &b);
        assert!(verify_mac(b"key", b"client", &a, &b, &tag));
        assert!(!verify_mac(b"key", b"client", &b, &a, &tag));
        assert!(!verify_mac(b"key", b"host", &a, &b, &tag));
    }
}
