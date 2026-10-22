# oriel

Low-latency remote desktop in Rust. One binary is both the host (shares a
display) and the client (connects to hosts).

- Hosts: Windows (Desktop Duplication + NVENC/AMF/QSV/Media Foundation via
  FFmpeg, zero-copy on NVIDIA and AMD) and Linux under GNOME (Mutter's
  remote desktop API + PipeWire, no permission prompt; VAAPI encoding).
- Clients: Linux (Wayland, X11), Windows; macOS should build but is untested.
- Client-side cursor: the host sends cursor images, the client's own OS cursor
  draws them, so pointing feels local even with latency. When an application
  on the host hides and confines the cursor (games), the client switches to
  relative mouse input automatically.
- A menu button opens quick actions (displays, full screen, statistics,
  Ctrl+Alt+Del, disconnect, settings); its shortcut (default
  **Ctrl+Alt+Shift+M**) opens the full menu. Drag it anywhere; Settings →
  Window picks its look (logo and shortcut, shortcut, logo) or hides it.
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
- Warnings that explain a slow stream while it lasts: a link that cannot
  carry the bitrate, packet loss, high latency, a host that stops
  answering, and slow encoding or decoding, each with what to try.
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
resolution = "2560x1440"

[shortcuts]         # defaults shown; "" disables one
fullscreen = "Ctrl+Alt+Shift+F"
next_display = "Ctrl+Alt+Shift+Right"
previous_display = "Ctrl+Alt+Shift+Left"
stats = "Ctrl+Alt+Shift+S"
disconnect = "Ctrl+Alt+Shift+Q"
```

## Build

```sh
nix develop
cargo build --release -p oriel          # Linux client (and test host)
./scripts/build-windows.sh --release    # Windows build in dist/windows
./scripts/package-windows.sh            # installer + portable ZIP in dist/
nix build                               # Nix package
```

See [docs/architecture.md](docs/architecture.md) for how it works and
[AGENTS.md](AGENTS.md) for development notes.
