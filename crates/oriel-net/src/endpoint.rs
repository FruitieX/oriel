use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rustls::{
    DigitallySignedStruct, DistinguishedName, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::{CryptoProvider, WebPkiSupportedAlgorithms},
    pki_types::{CertificateDer, ServerName, UnixTime},
    server::danger::{ClientCertVerified, ClientCertVerifier},
};

use crate::{Fingerprint, Identity};

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn transport_config() -> quinn::TransportConfig {
    let mut t = quinn::TransportConfig::default();
    // One stream per video frame; allow plenty in flight.
    t.max_concurrent_uni_streams(1024u32.into());
    t.max_concurrent_bidi_streams(16u32.into());
    // Room for intra-only frames at several hundred Mbit/s, but not for a
    // backlog of seconds of video (frames are paced by FrameAck anyway).
    t.stream_receive_window((16u32 * 1024 * 1024).into());
    t.receive_window((48u32 * 1024 * 1024).into());
    t.send_window(48 * 1024 * 1024);
    t.datagram_receive_buffer_size(Some(1024 * 1024));
    t.keep_alive_interval(Some(Duration::from_secs(2)));
    t.max_idle_timeout(Some(
        Duration::from_secs(10)
            .try_into()
            .expect("idle timeout fits"),
    ));
    t.initial_rtt(Duration::from_millis(10));
    t
}

#[derive(Debug)]
struct AcceptAnyCert {
    algorithms: WebPkiSupportedAlgorithms,
}

impl AcceptAnyCert {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            algorithms: provider().signature_verification_algorithms,
        })
    }
}

impl ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        // Checked by fingerprint after the handshake (see crate docs).
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

impl ClientCertVerifier for AcceptAnyCert {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

pub fn server_endpoint(identity: &Identity, listen: SocketAddr) -> Result<quinn::Endpoint> {
    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(AcceptAnyCert::new())
        .with_single_cert(vec![identity.cert.clone()], identity.key())?;
    tls.alpn_protocols = vec![oriel_proto::ALPN.to_vec()];
    tls.max_early_data_size = 0;
    let crypto = QuicServerConfig::try_from(tls)?;
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    config.transport_config(Arc::new(transport_config()));
    quinn::Endpoint::server(config, listen).with_context(|| format!("listen on {listen}"))
}

pub fn client_endpoint(identity: &Identity) -> Result<quinn::Endpoint> {
    let mut tls = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(AcceptAnyCert::new())
        .with_client_auth_cert(vec![identity.cert.clone()], identity.key())?;
    tls.alpn_protocols = vec![oriel_proto::ALPN.to_vec()];
    let crypto = QuicClientConfig::try_from(tls)?;
    let mut config = quinn::ClientConfig::new(Arc::new(crypto));
    config.transport_config(Arc::new(transport_config()));
    let mut endpoint = quinn::Endpoint::client((std::net::Ipv6Addr::UNSPECIFIED, 0).into())
        .or_else(|_| quinn::Endpoint::client((std::net::Ipv4Addr::UNSPECIFIED, 0).into()))
        .context("bind client socket")?;
    endpoint.set_default_client_config(config);
    Ok(endpoint)
}

/// Fingerprint of the certificate the peer presented during the handshake.
pub fn peer_fingerprint(conn: &quinn::Connection) -> Result<Fingerprint> {
    let identity = conn
        .peer_identity()
        .context("peer presented no certificate")?;
    let certs = identity
        .downcast::<Vec<CertificateDer<'static>>>()
        .map_err(|_| anyhow::anyhow!("unexpected peer identity type"))?;
    let cert = certs.first().context("peer presented no certificate")?;
    Ok(Fingerprint::of(cert))
}
