//! The hub's HTTP API (JSON), shared by the hub and the apps. The hub is
//! optional: devices that log in to one trust every device of the same
//! account, list them, and can ask it to relay a connection.

use serde::{Deserialize, Serialize};

/// Start logging this device in (OAuth device flow through the hub).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginStart {
    /// The device's identity fingerprint, hex.
    pub fingerprint: String,
    pub name: String,
    pub platform: String,
}

/// What to show the user: open `verification_uri` and enter `user_code`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginStarted {
    pub login_id: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
    /// Seconds between polls.
    pub interval: u64,
    pub expires_in: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginPoll {
    pub login_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum LoginStatus {
    Pending,
    Done {
        /// The device's credential for every later request (Bearer).
        token: String,
        device_id: String,
        /// Who logged in.
        user: String,
    },
    Failed {
        reason: String,
    },
}

/// Where a host accepts connections.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostInfo {
    pub port: u16,
    /// Its own addresses, most likely reachable first.
    pub addresses: Vec<String>,
}

/// Sent by each logged-in device every half minute or so.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Presence {
    pub name: String,
    pub platform: String,
    /// Set while the device shares itself.
    pub host: Option<HostInfo>,
    /// Firebase Cloud Messaging token (Android), to wake the device.
    #[serde(default)]
    pub push_token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    /// Identity fingerprint, hex.
    pub fingerprint: String,
    pub platform: String,
    pub host: Option<HostInfo>,
    /// Reported in within the last couple of minutes.
    pub online: bool,
    /// Unix seconds.
    pub last_seen: u64,
    /// The device asking.
    pub this_device: bool,
    /// The hub can wake it with a push.
    #[serde(default)]
    pub wakeable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Devices {
    pub devices: Vec<Device>,
    /// Who the account belongs to.
    #[serde(default)]
    pub user: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayRequest {
    pub device_id: String,
}

/// Send [`RELAY_HELLO`] + `token` from the socket the connection will use
/// to `address`; the hub answers [`RELAY_READY`] and from then on forwards
/// that socket's datagrams to the host and back (the QUIC connection stays
/// end-to-end encrypted).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Relay {
    pub address: String,
    /// Hex.
    pub token: String,
}

pub const RELAY_HELLO: &[u8] = b"ORIEL-RELAY1";
pub const RELAY_READY: &[u8] = b"ORIEL-RELAY-OK";

/// A request to wake a device (push) so it starts sharing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WakeRequest {
    pub device_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError {
    pub error: String,
}
