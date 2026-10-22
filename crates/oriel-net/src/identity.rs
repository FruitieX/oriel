use std::{fmt, fs, path::Path};

use anyhow::{Context, Result};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// SHA-256 of a certificate's DER encoding.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub struct Fingerprint(pub [u8; 32]);

impl Fingerprint {
    pub fn of(cert: &CertificateDer<'_>) -> Self {
        Self(Sha256::digest(cert.as_ref()).into())
    }

    /// Short form for display: the first 8 bytes in groups of two.
    pub fn short(&self) -> String {
        self.0[..8]
            .chunks(2)
            .map(hex::encode)
            .collect::<Vec<_>>()
            .join(":")
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fingerprint({})", self.short())
    }
}

impl From<Fingerprint> for String {
    fn from(fp: Fingerprint) -> String {
        fp.to_string()
    }
}

impl TryFrom<String> for Fingerprint {
    type Error = anyhow::Error;
    fn try_from(s: String) -> Result<Self> {
        let bytes = hex::decode(s.trim()).context("fingerprint is not hex")?;
        Ok(Self(bytes.try_into().map_err(|_| {
            anyhow::anyhow!("fingerprint must be 32 bytes")
        })?))
    }
}

/// This device's certificate and key.
pub struct Identity {
    pub cert: CertificateDer<'static>,
    key: Vec<u8>,
    pub fingerprint: Fingerprint,
}

impl Identity {
    pub fn load_or_create(dir: &Path) -> Result<Self> {
        let cert_path = dir.join("identity.crt.der");
        let key_path = dir.join("identity.key.der");
        if cert_path.exists() && key_path.exists() {
            let cert = CertificateDer::from(fs::read(&cert_path)?);
            let key = fs::read(&key_path)?;
            let fingerprint = Fingerprint::of(&cert);
            return Ok(Self {
                cert,
                key,
                fingerprint,
            });
        }
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let generated = rcgen::generate_simple_self_signed(vec!["oriel".to_string()])
            .context("generate identity certificate")?;
        let cert = CertificateDer::from(generated.cert.der().to_vec());
        let key = generated.signing_key.serialize_der();
        write_private(&key_path, &key)?;
        fs::write(&cert_path, cert.as_ref())?;
        let fingerprint = Fingerprint::of(&cert);
        tracing::info!(fingerprint = %fingerprint.short(), "created device identity");
        Ok(Self {
            cert,
            key,
            fingerprint,
        })
    }

    pub fn key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key.clone()))
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    #[cfg(unix)]
    {
        use std::{io::Write, os::unix::fs::OpenOptionsExt};
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(bytes)?;
    }
    #[cfg(not(unix))]
    fs::write(path, bytes)?;
    Ok(())
}
