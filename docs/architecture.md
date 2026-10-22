# Architecture

## Crates

| Crate | Role |
| --- | --- |
| `oriel-proto` | Wire protocol types, HID key table |
| `oriel-net` | Identity certificates, trust stores, QUIC endpoints, framing, PIN pairing |
| `oriel-media` | FFmpeg encoder/decoder (raw `ffmpeg-sys-next`) |
| `oriel-audio` | Audio capture (cpal, WASAPI loopback), Opus, jitter-buffered playback |
| `oriel-host` | Capture backends, input injection, capture→encode pipeline, sessions |
| `oriel-client` | Viewer window (winit, wgpu, egui), decoder thread, headless mode |
| `oriel` | The CLI binary |

## Connection

One QUIC connection (quinn, ALPN `oriel/1`) per session. Both sides present
self-signed certificates; TLS accepts any certificate (it still proves key
possession) and the application checks the peer's SHA-256 fingerprint
against the paired devices before doing anything else.

Pairing runs SPAKE2 with the host's six-digit PIN, then both sides exchange
HMACs over *both* certificate fingerprints keyed by the SPAKE2 secret. A man
in the middle sees different certificates on each leg, so the HMACs fail.
The host disables pairing after five wrong PINs.

Streams:

- **Control** (bidirectional, client-opened): length-prefixed postcard
  `ClientMsg`/`HostMsg`.
- **Input** (unidirectional, client-opened, highest priority): ordered
  `InputEvent`s. Moves and clicks must stay ordered, so input is reliable.
- **Video**: one unidirectional stream per frame, opened in frame order with
  descending priority so older frames finish first. The client reads them
  sequentially (inter-coded video must be decoded in order).
- **Datagrams**: cursor positions and audio host→client, `FrameAck`
  client→host.

## Pacing

The host encodes a frame only when the screen changed, the frame-rate slot
has come, and fewer than `ceil(RTT / frame interval) + 2` frames are
unacknowledged. Acknowledgement is the client's `FrameAck` datagram after
decoding (QUIC acks can be delayed by up to 25 ms) or the stream completing.
Skipped changes are not lost: the capturer keeps the latest image and it is
encoded at the next opportunity. Encoders run with no B-frames and an
effectively infinite GOP; keyframes come at stream start and on request
(decode errors).

## Windows host

- Desktop Duplication on the adapter that owns the output; each frame is
  copied into an owned texture and released immediately.
- NVENC and AMF get that texture zero-copy through an FFmpeg D3D11 frames
  context (BGRA; the encoder converts to YUV and signals BT.601 limited in
  the VUI). Other encoders get a CPU readback converted with swscale
  (BT.709 limited).
- The cursor image comes from Desktop Duplication; "captured" means hidden
  (`GetCursorInfo`) and clipped (`GetClipCursor`) away from the full virtual
  desktop.
- Input uses `SendInput` with set-1 scancodes so the host's layout applies,
  and follows the input desktop (lock screen, UAC) when allowed.

## Client

The decoder thread prefers VAAPI (Linux), D3D11VA (Windows) or VideoToolbox
(macOS), downloads to NV12/I420 and publishes only the newest frame. The
renderer uploads the planes and converts with the frame's own matrix and
range. The window cursor is set to the host's cursor image; in relative mode
the pointer is locked and raw motion is sent.
