//! Networking for oriel: device identity, trust stores, QUIC endpoints,
//! message framing and PIN pairing.
//!
//! Every installation has a self-signed certificate. TLS accepts any peer
//! certificate (the handshake still proves key ownership); the application
//! then checks the peer's SHA-256 fingerprint against the paired devices
//! before sending or acting on anything else. Pairing binds both
//! fingerprints to a short PIN with SPAKE2, so a man in the middle cannot
//! complete it.

pub mod config;
pub mod endpoint;
pub mod framing;
pub mod identity;
pub mod pairing;

pub use config::{Paths, TrustStore, TrustedPeer};
pub use endpoint::{client_endpoint, peer_fingerprint, server_endpoint};
pub use framing::{ControlStream, read_msg, write_msg};
pub use identity::{Fingerprint, Identity};
