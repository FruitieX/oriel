//! Wire protocol shared by the oriel host and client.
//!
//! A session is one QUIC connection (ALPN [`ALPN`]) carrying:
//! - one bidirectional *control* stream opened by the client, with
//!   length-prefixed [`ClientMsg`] / [`HostMsg`] messages;
//! - one unidirectional *input* stream opened by the client, carrying
//!   [`InputEvent`]s in order (moves and clicks must not be reordered);
//! - one unidirectional stream per encoded video frame, opened by the host,
//!   starting with a [`FrameHeader`] followed by the bitstream;
//! - unreliable datagrams for state where only the latest value matters
//!   ([`Datagram`]).
//!
//! Everything is encoded with postcard. Video is organised around
//! *surfaces*: today a surface is a whole display, later also single windows.

use serde::{Deserialize, Serialize};

pub mod keys;

pub const ALPN: &[u8] = b"oriel/1";
pub const PROTOCOL_VERSION: u32 = 2;
pub const DEFAULT_PORT: u16 = 48600;
/// Upper bound for a single control/input message.
pub const MAX_MESSAGE_LEN: usize = 4 * 1024 * 1024;

pub type SurfaceId = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Codec {
    H264,
    Hevc,
    Av1,
    /// Intra-only wavelet codec for very high bandwidth links.
    PyroWave,
}

impl Codec {
    pub fn name(self) -> &'static str {
        match self {
            Codec::H264 => "H.264",
            Codec::Hevc => "HEVC",
            Codec::Av1 => "AV1",
            Codec::PyroWave => "PyroWave",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayInfo {
    pub id: u32,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub primary: bool,
    /// Resolutions the host can switch this display to (empty: cannot).
    pub modes: Vec<(u32, u32)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamSettings {
    /// Display to stream; `None` picks the primary display.
    pub display: Option<u32>,
    /// Codecs the client can decode, in order of preference.
    pub codecs: Vec<Codec>,
    /// Target bitrate in kbit/s. 0 lets the host decide.
    pub bitrate_kbps: u32,
    /// Maximum frame rate.
    pub max_fps: u32,
    /// Stream the host's sound.
    pub audio: bool,
    /// Let the host switch between the listed codecs as the link allows:
    /// PyroWave on fast, low-latency links, the next codec otherwise.
    pub auto_codec: bool,
    /// Bitrate for PyroWave in kbit/s (it needs far more than H.26x).
    pub pyrowave_kbps: u32,
    /// Switch the host display to this resolution for the session.
    pub resolution: Option<(u32, u32)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SurfaceInfo {
    pub id: SurfaceId,
    pub width: u32,
    pub height: u32,
    pub codec: Codec,
}

/// Pairing messages, run over the control stream before a session when the
/// client does not yet know the host (or vice versa).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PairMsg {
    /// SPAKE2 message keyed with the PIN shown on the host.
    Spake(Vec<u8>),
    /// HMAC over both certificate fingerprints, keyed with the SPAKE2 secret.
    Confirm(Vec<u8>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientMsg {
    Hello {
        protocol: u32,
        name: String,
        /// Request pairing instead of a session.
        pair: bool,
    },
    Pair(PairMsg),
    StartStream(StreamSettings),
    RequestKeyframe,
    /// Switch the stream to another display.
    SelectDisplay {
        id: u32,
    },
    SetBitrate {
        kbps: u32,
    },
    /// Change the codec preference (as in `StreamSettings`).
    SetCodecs {
        codecs: Vec<Codec>,
        auto_codec: bool,
    },
    /// Change (or with `None` restore) the host display resolution.
    SetResolution {
        resolution: Option<(u32, u32)>,
    },
    Ping {
        id: u64,
    },
    /// The client's clipboard changed (text only).
    Clipboard {
        text: String,
    },
    /// Sent on graceful disconnect.
    Bye,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HostMsg {
    Welcome {
        protocol: u32,
        name: String,
        displays: Vec<DisplayInfo>,
        /// Codecs the host can encode.
        codecs: Vec<Codec>,
    },
    Pair(PairMsg),
    PairResult {
        ok: bool,
        message: String,
    },
    /// Refused (unpaired client, version mismatch, ...). The host closes the
    /// connection after this.
    Rejected {
        reason: String,
    },
    SurfaceStarted(SurfaceInfo),
    SurfaceEnded {
        id: SurfaceId,
    },
    CursorShape(CursorShape),
    /// Reliable cursor state change (visibility / capture). Positions travel
    /// as datagrams.
    CursorState(CursorState),
    Pong {
        id: u64,
        /// Host clock (the same clock as `FrameHeader::capture_us`).
        host_us: u64,
    },
    /// The host's clipboard changed (text only).
    Clipboard {
        text: String,
    },
    /// Human-readable notice, e.g. "secure desktop active".
    Notice {
        text: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CursorShape {
    /// Hash of the image, so clients can cache shapes.
    pub id: u64,
    pub width: u32,
    pub height: u32,
    pub hotspot_x: u32,
    pub hotspot_y: u32,
    /// Straight (non-premultiplied) RGBA8.
    pub rgba: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorState {
    pub surface: SurfaceId,
    pub visible: bool,
    /// An application on the host has hidden and confined the cursor (games,
    /// 3D viewports): the client should switch to relative mouse input.
    pub captured: bool,
    pub shape: u64,
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum InputEvent {
    /// Absolute position in surface pixels.
    MouseMove {
        surface: SurfaceId,
        x: i32,
        y: i32,
    },
    /// Relative motion in device units (used while the cursor is captured).
    MouseMotion {
        dx: i32,
        dy: i32,
    },
    MouseButton {
        button: MouseButton,
        down: bool,
    },
    /// Wheel motion in 1/120ths of a notch (Windows WHEEL_DELTA units).
    MouseWheel {
        dx: i32,
        dy: i32,
    },
    /// USB HID usage (page 0x07) of the physical key.
    Key {
        usage: u16,
        down: bool,
    },
    /// Release every key and button the client may have left pressed (focus
    /// loss, menu open).
    ReleaseAll,
    /// Ctrl+Alt+Del (the secure attention sequence).
    SecureAttention,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Datagram {
    /// Host → client: where the host's cursor is.
    CursorPosition { surface: SurfaceId, x: i32, y: i32 },
    /// Client → host: every frame up to `seq` has been decoded. Paces the
    /// host's encoder (QUIC acknowledgements can be delayed by up to 25 ms).
    FrameAck { surface: SurfaceId, seq: u64 },
    /// Host → client: one 10 ms Opus packet (48 kHz stereo).
    Audio { seq: u32, data: Vec<u8> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameHeader {
    pub surface: SurfaceId,
    pub seq: u64,
    /// Host capture time, microseconds on the host's monotonic clock.
    pub capture_us: u64,
    /// Time spent encoding, microseconds.
    pub encode_us: u32,
    pub keyframe: bool,
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("message too large ({0} bytes)")]
    TooLarge(usize),
    #[error("decode: {0}")]
    Decode(#[from] postcard::Error),
}

/// Serialise `msg` with a u32 little-endian length prefix.
pub fn encode_framed<T: Serialize>(msg: &T) -> Result<Vec<u8>, ProtoError> {
    let body = postcard::to_stdvec(msg)?;
    if body.len() > MAX_MESSAGE_LEN {
        return Err(ProtoError::TooLarge(body.len()));
    }
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, ProtoError> {
    Ok(postcard::to_stdvec(msg)?)
}

pub fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, ProtoError> {
    Ok(postcard::from_bytes(bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framed_roundtrip() {
        let msg = ClientMsg::StartStream(StreamSettings {
            display: None,
            codecs: vec![Codec::Hevc, Codec::H264],
            bitrate_kbps: 50_000,
            max_fps: 120,
            audio: true,
            auto_codec: false,
            pyrowave_kbps: 400_000,
            resolution: None,
        });
        let bytes = encode_framed(&msg).unwrap();
        let len = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        assert_eq!(len, bytes.len() - 4);
        let back: ClientMsg = decode(&bytes[4..]).unwrap();
        assert!(matches!(back, ClientMsg::StartStream(s) if s.max_fps == 120));
    }
}
