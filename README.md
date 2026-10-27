# oriel

Low-latency remote desktop in Rust. One binary is both the host (shares a
display) and the client (connects to hosts).

- Hosts: Windows (Desktop Duplication + NVENC/AMF/QSV/Media Foundation via
  FFmpeg, zero-copy on NVIDIA and AMD) and Linux under GNOME (Mutter's
  remote desktop API + PipeWire, no permission prompt; VAAPI encoding).
- Clients: Linux (Wayland, X11), Windows, Android, macOS (an app bundle
  from `scripts/package-macos.sh`).
- Client-side cursor: the host sends cursor images, the client's own OS cursor
  draws them, so pointing feels local even with latency. When an application
  on the host hides and confines the cursor (games), the client switches to
  relative mouse input automatically.
- A menu button opens quick actions (displays, full screen, statistics,
  Ctrl+Alt+Del, disconnect, settings); its shortcut (default
  **Ctrl+Alt+Shift+M**) opens the full menu. Drag it anywhere; Settings →
  Interface picks its look (logo and shortcut, shortcut, logo) and size, or
  hides it. While either is open, keys and shortcuts (Alt+Tab) stay local.
- The host's display can switch to a resolution for the session: one it
  lists, or this computer's screen scaled 10–100 % (the closest mode the
  display supports, preferring the same shape). Windows hosts set it again
  when a monitor that slept comes back in its own mode.
- System audio (Opus) from the host output you pick in the menu, played on
  the local output chosen in Settings; text clipboard sync; display
  selection.
- Clipboard sync of text and images (images as PNG, up to 24 MB).
- Microphone (off by default; Settings → Sound, or the menu): the host
  sees it as "oriel microphone" on Linux, and on Windows as the recording
  end of Steam Streaming Microphone or VB-Audio Virtual Cable.
- Gamepads: the client's controllers appear on the host as Xbox 360 pads,
  with rumble, while the viewer has focus. Windows hosts need the
  [ViGEmBus](https://github.com/nefarius/ViGEmBus/releases) driver; Linux
  hosts need write access to `/dev/uinput`.
- Use the client as an extra display: the host creates a virtual monitor
  in the mode you pick in the menu (default: the client's screen). GNOME
  hosts work as they are; Windows hosts need the
  [SudoVDA](https://github.com/SudoMaker/SudoVDA) driver (installed by
  [Apollo](https://github.com/ClassicOldSong/Apollo/releases)'s installer;
  oriel only needs the driver, not Apollo). The client draws the host's
  cursor when the host's own mouse moves it onto that display.
- Shortcuts for the menu, fullscreen, next/previous display, statistics and
  disconnecting; rebind them in Settings → Input.
- Codecs: HEVC/H.264/AV1 through FFmpeg, and
  [PyroWave](https://github.com/Themaister/pyrowave) for fast LANs
  (`--codec pyrowave`, or `--codec auto` to switch between PyroWave and HEVC
  as the link allows).
- Automatic bitrate (on by default): the host lowers the bitrate within a
  fraction of a second when the round trip grows, frames wait on the client
  or packets get lost, and raises it again up to the chosen bitrate once the
  link is clean. NVENC, Quick Sync and x264 change it on the fly; other
  encoders are reopened (a keyframe), at most every 2 s. PyroWave keeps its
  own fallback to HEVC.
- Smoothness (Settings → Video): off by default for the lowest latency;
  Automatic or a fixed 10–100 ms holds frames so that network jitter does
  not make motion stutter, showing each frame at its capture time plus the
  buffer. The statistics show the buffer and how evenly frames are shown.
- Warnings that explain a slow stream while it lasts: a link that cannot
  carry the bitrate, packet loss, jitter, high latency, sound dropouts, a
  host that stops answering, slow encoding or decoding, and a host on
  another oriel version (which still connects), each with what to try
  (Settings → Interface: detailed, short or off).
  `ORIEL_DEBUG_BAD_NETWORK=1` fakes a bad network to see them.
- Only paired devices can connect: pairing uses a PIN shown on the host and
  pins both devices' certificates.

## Use

```sh
# On the host (Windows: oriel.exe):
oriel host --pair            # prints a PIN; allow UDP 48600 in the firewall

# On the client:
oriel pair <host-address> --pin <PIN>
oriel connect <host-name>    # --stats, --fullscreen, --codec hevc,h264, ...
```

On Windows, install with `oriel-VERSION-windows-x64-setup.exe` (from
`scripts/package-windows.sh`, see below). It installs for every user in
Program Files and, by default, the hosting service (shares this computer
from startup, including the sign-in, lock and UAC screens, with a firewall
rule for UDP 48600 on private networks, and for the local subnet on
networks Windows calls Public) and the tray app at sign-in. The
app then shows and controls the service: the user who installed it can
start and stop sharing and allow pairing without a UAC prompt (others are
asked for administrator rights), and quitting the app leaves the service
running. Each pairing PIN pairs one device within ten minutes. The service
takes over the installing user's identity and paired clients, so clients
paired before keep working. A portable ZIP is built too.

Without the installer, the service can be set up by hand (in an
Administrator terminal):

```powershell
oriel.exe service install      # copies to Program Files, opens UDP 48600
oriel.exe service pair         # prints a single-use PIN valid for ten minutes
```

The service keeps its own identity and pairings in `C:\ProgramData\oriel`
(logs: `oriel-service.log`, `oriel-host.log`).

Without `--pair`, the host only accepts already paired clients. `oriel hosts`,
`oriel clients`, `oriel forget`, `oriel unpair` manage pairings; `oriel info`
shows the identity, encoders and displays.

Settings are one set of defaults plus per-computer overrides, edited in
the same panel everywhere: Settings in the start window (the defaults),
a computer tile's ⋯ menu → Settings (that computer), and the menu while
connected (that computer, applied immediately). Settings → App can start
oriel at login (in the tray on Windows, minimized elsewhere). They are
stored in `client.toml` in the config directory (`oriel info` prints it),
overrides under `[hosts.<name>]`:

```toml
menu_shortcut = "Ctrl+Alt+Shift+M"
bitrate_kbps = 50000
max_fps = 120
codecs = ["hevc", "h264"]
mouse = "auto"      # or "desktop", "captured"
audio = true

[hosts.watermelon]  # overrides for one computer
max_fps = 144
resolution = "2560x1440"  # or "client" with resolution_scale = 0.5

[shortcuts]         # defaults shown; "" disables one
fullscreen = "Ctrl+Alt+Shift+F"
next_display = "Ctrl+Alt+Shift+Right"
previous_display = "Ctrl+Alt+Shift+Left"
stats = "Ctrl+Alt+Shift+S"
disconnect = "Ctrl+Alt+Shift+Q"
```

## Hub (optional)

Instead of pairing every client with every host, devices can log in to a
hub (Settings → App → Account): every logged-in device reaches every
computer shared under the account, and connections that cannot be made
directly go through the hub's relay. See [docs/hub.md](docs/hub.md).

## Android

The Android app (Android 9 and newer) connects to hosts; it does not
share the phone. Video decodes in hardware (MediaCodec, asking for
low-latency decoding); PyroWave is desktop-only.

- Tap: click. Drag: hold the left button. Long press or two-finger tap:
  right click.
- Two fingers pinch to zoom and pan the picture (zoom is local; the
  quick menu resets it); while the whole picture fits, they scroll. Three
  fingers scroll.
- The keyboard button (bottom left) shows the phone's keyboard; what it
  types goes to the host as text, whatever the host's keyboard layout. A
  row above it adds Esc, Tab, arrows and Ctrl/Alt/Shift/Win, which latch
  for the next key (Ctrl, then C).
- Back opens or closes the menu; "Fill the screen" hides the system bars.
- The clipboard's text syncs both ways. Gamepads and the microphone are
  not supported on Android yet.

## Build

```sh
nix develop
cargo build --release -p oriel          # Linux client (and test host)
./scripts/build-windows.sh --release    # Windows build in dist/windows
./scripts/package-windows.sh            # installer + portable ZIP in dist/
./scripts/package-macos.sh              # on a Mac: oriel.app + ZIP in dist/
nix build                               # Nix package

nix develop .#android
./scripts/build-android.sh [--install]  # debug APK in dist/ (arm64 + x86_64)
./scripts/android-release.sh            # release APK, signed with the release key
```

See [docs/architecture.md](docs/architecture.md) for how it works and
[AGENTS.md](AGENTS.md) for development notes.
