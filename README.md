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
- A menu button in the bottom-right corner shows the shortcut
  (default **Ctrl+Alt+Shift+M**) that opens the menu and releases keyboard
  and mouse.
- System audio (Opus) from the host output you pick in the menu, played on
  the local output chosen in Settings; text clipboard sync; display
  selection.
- Gamepads: the client's controllers appear on the host as Xbox 360 pads,
  with rumble, while the viewer has focus. Windows hosts need the
  [ViGEmBus](https://github.com/nefarius/ViGEmBus/releases) driver; Linux
  hosts need write access to `/dev/uinput`.
- Shortcuts for the menu, fullscreen, next/previous display, statistics and
  disconnecting; rebind them in Settings → Input.
- Codecs: HEVC/H.264/AV1 through FFmpeg, and
  [PyroWave](https://github.com/Themaister/pyrowave) for fast LANs
  (`--codec pyrowave`, or `--codec auto` to switch between PyroWave and HEVC
  as the link allows).
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

To reach the lock screen, UAC prompts and the login screen, run the host as
a Windows service instead (in an Administrator terminal):

```powershell
oriel.exe service install      # copies to Program Files, opens UDP 48600
oriel.exe service pair         # prints a PIN valid for ten minutes
```

The service keeps its own identity and pairings in `C:\ProgramData\oriel`
(logs: `oriel-service.log`, `oriel-host.log`).

Without `--pair`, the host only accepts already paired clients. `oriel hosts`,
`oriel clients`, `oriel forget`, `oriel unpair` manage pairings; `oriel info`
shows the identity, encoders and displays.

Client defaults can go in `client.toml` in the config directory
(`oriel info` prints it):

```toml
menu_shortcut = "Ctrl+Alt+Shift+M"
bitrate_kbps = 50000
max_fps = 120
codecs = ["hevc", "h264"]
mouse = "auto"      # or "desktop", "captured"
audio = true

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
nix build                               # Nix package
```

See [docs/architecture.md](docs/architecture.md) for how it works and
[AGENTS.md](AGENTS.md) for development notes.
