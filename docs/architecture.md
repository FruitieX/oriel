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

## Linux host (GNOME)

Mutter's own D-Bus API (`org.gnome.Mutter.RemoteDesktop` + `ScreenCast`, as
used by gnome-remote-desktop) gives a screen cast of a monitor linked to an
input session, with no permission dialog. One session is shared by the
capturer and the injector. Frames arrive over PipeWire as BGRx in shared
memory with the cursor as `SPA_META_Cursor` metadata (client-side cursor);
input goes through `Notify*` calls with evdev key and button codes. System
sound is a PipeWire capture of the default sink. Other desktops would use
xdg-desktop-portal (ScreenCast + RemoteDesktop) behind the same interfaces.

Testing without a desktop: run PipeWire, WirePlumber (plus a null sink for
audio) and `gnome-shell --headless --wayland --virtual-monitor 1920x1080`
inside `dbus-run-session`, then `oriel host` there (`XDG_CURRENT_DESKTOP=GNOME`).

## Client

The decoder thread prefers VAAPI (Linux), D3D11VA (Windows) or VideoToolbox
(macOS), downloads to NV12/I420 and publishes only the newest frame. The
renderer uploads the planes and converts with the frame's own matrix and
range. The window cursor is set to the host's cursor image; in relative mode
the pointer is locked and raw motion is sent.

## PyroWave and automatic codec switching

PyroWave is loaded at runtime from its C API library (built by
`nix/pyrowave.nix`; `ORIEL_PYROWAVE_LIB` overrides the path). A frame's
payload is its PyroWave packets, each prefixed with a little-endian `u32`
length. Output is full-range BT.709 4:2:0.

- Windows host (zero-copy): the captured texture is copied on the GPU into
  a D3D11 texture shared by NT handle with a PyroWave Vulkan device on the
  same adapter (LUID); a shared D3D11 fence imported as a timeline
  semaphore orders the copy before `encode_gpu_scaled`, which converts
  BGRA on the GPU.
- Client (zero-copy, Vulkan): plane textures are created in wgpu's Vulkan
  device with exportable memory, imported into a PyroWave device on the
  same GPU (device/driver UUID) and decoded into directly (a ring of three
  sets; the decoder thread waits on a timeline semaphore before handing a
  set to the renderer).
- Otherwise frames go through the CPU (swscale BGRA→YUV420P and
  `encode_cpu`; `decode_cpu_buffer_synchronous`).

With `auto_codec`, the host uses PyroWave while the RTT is below 3 ms and
falls back to the next listed codec when fewer than 75% of the captured
frames could be sent for two seconds in a row because frames were waiting on
the client. It retries PyroWave after 30 s, doubling up to 10 min.

## Windows service

`oriel service install` registers `oriel service run` as a LocalSystem
service. The service (session 0) starts `oriel host` in the active console
session with a duplicate of its SYSTEM token, restarts it when it exits or
the console session changes, and calls `SendSAS` when the host signals the
`Global\oriel-secure-attention` event. As SYSTEM the host can open the
Winlogon desktop, so capture and input follow the lock screen and UAC
prompts. Pairing is allowed for ten minutes after `oriel service pair`
writes a PIN to `pairing-pin` in the data directory.

## App, tray and development hosts

`oriel` is one app: the launcher window, the viewer, and (on Windows) a
tray icon owned by the same winit event loop. Closing the window keeps the
app in the tray; the tray lists connected clients (with disconnect),
toggles sharing and allows pairing. Sharing runs the host in-process
(`HostControl` exposes status, clients, kick and pairing). `oriel host` on
Windows is this app started hidden with sharing on.

Development hosts run with `--update-from <dir>`: when a new `oriel.exe`
appears there, the running copy renames its own files aside, copies the new
build next to itself and relaunches. `dist/windows/run-host.bat` installs
the build into `%LOCALAPPDATA%\oriel-dev` and starts it that way, so
`scripts/build-windows.sh` deploys to the test PC without user action.
