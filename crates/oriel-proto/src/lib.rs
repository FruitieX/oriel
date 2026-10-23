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
/// A client closes a connection with this reason right after the handshake
/// when it only checks whether the host is up.
pub const PROBE_REASON: &[u8] = b"probe";

pub const PROTOCOL_VERSION: u32 = 11;
pub const DEFAULT_PORT: u16 = 48600;
/// Upper bound for a single control/input message.
pub const MAX_MESSAGE_LEN: usize = 32 * 1024 * 1024;

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

/// A monitor the host creates for the client (extending its desktop).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VirtualMode {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
}

/// A host sound output that can be streamed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioDevice {
    pub id: String,
    pub name: String,
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
    /// The host output whose sound is streamed (an [`AudioDevice::id`]);
    /// `None` follows the host's default output.
    pub audio_device: Option<String>,
    /// Stream a new virtual monitor in this mode instead of `display`.
    pub virtual_display: Option<VirtualMode>,
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
    /// Stream another host output's sound (as in `StreamSettings`).
    SetAudioDevice {
        id: Option<String>,
    },
    /// Switch to a virtual monitor in this mode (or change its mode), or
    /// with `None` back to the physical display.
    SetVirtualDisplay {
        mode: Option<VirtualMode>,
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
    /// A gamepad went away; the host unplugs its virtual pad.
    GamepadRemoved {
        pad: u8,
    },
    /// The client's clipboard now holds this image (PNG).
    ClipboardImage {
        png: Vec<u8>,
    },
    /// Change the frame rate limit.
    SetMaxFps {
        fps: u32,
    },
    /// Change PyroWave's bitrate.
    SetPyroWaveBitrate {
        kbps: u32,
    },
    /// Start or stop streaming sound.
    SetAudio {
        enabled: bool,
    },
    /// Follow the network below the chosen bitrate (on), or keep to it.
    SetAutoBitrate {
        enabled: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HostMsg {
    Welcome {
        protocol: u32,
        name: String,
        displays: Vec<DisplayInfo>,
        /// Codecs the host can encode.
        codecs: Vec<Codec>,
        /// Sound outputs the client can choose to hear.
        audio_devices: Vec<AudioDevice>,
        /// The host can create virtual monitors.
        virtual_display: bool,
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
    /// The host's clipboard now holds this image (PNG).
    ClipboardImage {
        png: Vec<u8>,
    },
    /// Set a gamepad's rumble motors (0 stops them) until the next update.
    Rumble {
        pad: u8,
        strong: u16,
        weak: u16,
    },
    /// Once a second while streaming: how well the host keeps up.
    Health(HostHealth),
}

/// What only the host can see about a stream's last second.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct HostHealth {
    /// Frames sent.
    pub sent_fps: u16,
    /// Frames the screen changed (up to the frame rate limit).
    pub wanted_fps: u16,
    /// Frames waited on the network: the link cannot carry the stream.
    pub network_limited: bool,
    /// Packets lost on the way to the client, per mille.
    pub loss_permille: u16,
    /// The bitrate the encoder aims for (below the client's choice with
    /// automatic bitrate), kbit/s.
    pub bitrate_kbps: u32,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// Type this text, whatever the host's keyboard layout (a phone's
    /// keyboard sends text, not keys). "\n" is Enter, "\t" Tab and
    /// "\u{8}" Backspace.
    Text {
        text: String,
    },
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
    /// Client → host: one 10 ms Opus packet from the client's microphone.
    Microphone { seq: u32, data: Vec<u8> },
    /// Client → host: a gamepad's full state, sent on change and every
    /// 100 ms; `seq` (per pad) discards reordered ones.
    Gamepad {
        pad: u8,
        seq: u32,
        state: GamepadState,
    },
}

/// A gamepad in XInput terms (Xbox layout).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GamepadState {
    /// [`gamepad`] button bits.
    pub buttons: u16,
    pub left_trigger: u8,
    pub right_trigger: u8,
    /// Sticks; positive is right and up.
    pub lx: i16,
    pub ly: i16,
    pub rx: i16,
    pub ry: i16,
}

/// Gamepad button bits (the XInput values).
pub mod gamepad {
    pub const DPAD_UP: u16 = 0x0001;
    pub const DPAD_DOWN: u16 = 0x0002;
    pub const DPAD_LEFT: u16 = 0x0004;
    pub const DPAD_RIGHT: u16 = 0x0008;
    pub const START: u16 = 0x0010;
    pub const BACK: u16 = 0x0020;
    pub const LEFT_THUMB: u16 = 0x0040;
    pub const RIGHT_THUMB: u16 = 0x0080;
    pub const LEFT_SHOULDER: u16 = 0x0100;
    pub const RIGHT_SHOULDER: u16 = 0x0200;
    pub const GUIDE: u16 = 0x0400;
    pub const A: u16 = 0x1000;
    pub const B: u16 = 0x2000;
    pub const X: u16 = 0x4000;
    pub const Y: u16 = 0x8000;
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

/// The first byte of an encoded [`ClientMsg::StartStream`] in every
/// protocol version, so a host can tell when it cannot read one.
pub const START_STREAM_TAG: u8 = 2;

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
            audio_device: None,
            virtual_display: None,
        });
        let bytes = encode_framed(&msg).unwrap();
        let len = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        assert_eq!(len, bytes.len() - 4);
        let back: ClientMsg = decode(&bytes[4..]).unwrap();
        assert!(matches!(back, ClientMsg::StartStream(s) if s.max_fps == 120));
        assert_eq!(encode(&msg).unwrap()[0], START_STREAM_TAG);
    }
}
