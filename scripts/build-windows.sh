#!/usr/bin/env bash
# Cross-compile oriel for Windows (x86_64-pc-windows-gnu) and assemble a
# runnable folder in dist/windows with the FFmpeg DLLs next to oriel.exe.
# Run inside `nix develop`. Extra arguments go to cargo (e.g. --release).
set -euo pipefail
cd "$(dirname "$0")/.."

: "${ORIEL_FFMPEG_WIN64:?run inside nix develop}"
target=x86_64-pc-windows-gnu

# ffmpeg-sys-next reads FFMPEG_DIR for the target build; bindgen needs the
# mingw headers to parse FFmpeg's.
export FFMPEG_DIR="$ORIEL_FFMPEG_WIN64"
export BINDGEN_EXTRA_CLANG_ARGS_x86_64_pc_windows_gnu="--target=x86_64-w64-mingw32 -D__IMMINTRIN_H -D__X86INTRIN_H $("${CC_x86_64_pc_windows_gnu}" -E -Wp,-v -x c /dev/null 2>&1 | grep -v '/lib/gcc/' | sed -n 's/^ \(\/nix.*include.*\)$/-isystem \1/p' | tr '\n' ' ')"

profile=debug
for a in "$@"; do [[ "$a" == "--release" ]] && profile=release; done

cargo build --target "$target" -p oriel "$@"

out=dist/windows
mkdir -p "$out"
cp "target/$target/$profile/oriel.exe" "$out/"
cp "$ORIEL_FFMPEG_WIN64"/bin/{avcodec,avutil,swscale,swresample}-*.dll "$out/"
cp "$ORIEL_MINGW_PTHREADS"/bin/libwinpthread-1.dll "$out/" 2>/dev/null || true
cp "$ORIEL_PYROWAVE_WIN64"/bin/libpyrowave-shared-1.dll "$out/"
chmod -R u+w,+x "$out"
echo "built $out/oriel.exe"
