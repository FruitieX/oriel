# oriel development notes

Read [docs/architecture.md](docs/architecture.md) first.

## Environment

- Everything runs in `nix develop` (Rust with the Windows target, FFmpeg 8,
  mingw, Wine). The mingw compiler is deliberately not on
  `nativeBuildInputs`: its setup hook would set `CC` for native builds.
- `scripts/build-windows.sh [--release]` cross-builds into `dist/windows`
  with the FFmpeg DLLs. Bindgen for the Windows FFmpeg headers needs the
  mingw include paths but not GCC's intrinsics headers, and must skip
  clang's `immintrin.h`/`x86intrin.h` (AMX types break the generated
  bindings); the script handles both. If bindings look stale, `cargo clean
  -p ffmpeg-sys-next --target x86_64-pc-windows-gnu`.
- Windows is tested on the owner's PC (watermelon) through the Samba
  share: `dist/windows` is `Y:\projects\oriel\dist\windows` there. Once
  the owner has started `run-host.bat`, every `build-windows.sh` run
  redeploys automatically (the host restarts into the new build within
  seconds) and the host logs to `dist/host.log`. honeydew is paired with it
  as `watermelon`, so `oriel connect watermelon --headless 5` (with that
  client's `ORIEL_CONFIG_DIR`) tests the real Windows host from here. Only
  release builds go to `dist/` (debug builds overflowed the Windows main
  thread stack).

## Testing without hardware

- `oriel host --test-pattern` streams a synthetic pattern that draws the
  input it receives (F9 toggles the "captured cursor" state).
- Use separate `ORIEL_CONFIG_DIR`s for host and client on one machine.
- `oriel connect <host> --headless N` streams without a window and prints
  stats once a second. It moves the host's mouse to exercise input; set
  `ORIEL_HEADLESS_STILL=1` against a real desktop, where hover effects
  would otherwise keep the screen changing.
- The window can be tested under a headless Weston
  (`weston --backend=headless --renderer=gl --socket=...`) with
  `--screenshot path.png`; `ORIEL_DEBUG_MENU=1` opens the menu at start.
- Machines without a sound card: point `HOME` at a directory whose
  `.asoundrc` sets `pcm.!default { type null }`.
- Gamepads: the uinput tests (`gamepad` in oriel-host and oriel-client)
  need write access to `/dev/uinput` and skip without it. honeydew has
  none; run the test binaries as root on a machine that does.
  `ORIEL_DEBUG_GAMEPAD=1` makes `--headless` send a synthetic controller.
- Audio device routing: run a private `pipewire` and `wireplumber` (in
  `dbus-run-session`) with null sinks `oriel-sink-a`/`oriel-sink-b`, then
  `ORIEL_AUDIO_DEVICE_TEST=1 cargo test -p oriel-audio routing`. Keep
  `XDG_RUNTIME_DIR` short: PipeWire fails with "File name too long" on
  long socket paths.
- Do not test `av1_vaapi` on the Radeon 780M (honeydew): Mesa accepts the
  encoder but the GPU context is reset.

## Conventions

- Media code calls FFmpeg through raw FFI; check field layouts against the
  FFmpeg version in the flake (8.1) when touching hwcontext structs.
- Protocol changes need both ends rebuilt; there is no compatibility layer
  beyond `PROTOCOL_VERSION`.
