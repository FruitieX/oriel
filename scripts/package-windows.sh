#!/usr/bin/env bash
# Windows packages: a release build, then the installer (NSIS) and a
# portable ZIP in dist/, with SHA256SUMS.txt. Run inside `nix develop`.
set -euo pipefail
cd "$(dirname "$0")/.."

./scripts/build-windows.sh
version=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml)
[ -n "$version" ] || { echo "no version in Cargo.toml" >&2; exit 1; }

stage=target/package/oriel
rm -rf target/package
mkdir -p "$stage/licenses"
cp dist/windows/*.exe dist/windows/*.dll "$stage/"
cp LICENSE-MIT LICENSE-APACHE packaging/windows/THIRD-PARTY-NOTICES.txt "$stage/licenses/"
chmod u+w "$stage/licenses/THIRD-PARTY-NOTICES.txt"
cp "$ORIEL_FFMPEG_WIN64/LICENSE.txt" "$stage/licenses/FFmpeg-LICENSE.txt"
# PyroWave and winpthreads, from Nix (scripts/third-party-notices.sh).
./scripts/third-party-notices.sh "$(nix build --no-link --print-out-paths .#third-party-meta-windows)" \
  "$stage/licenses" "$ORIEL_PYROWAVE_WIN64/bin/libpyrowave-shared-1.dll" \
  "$ORIEL_MINGW_PTHREADS/bin/libwinpthread-1.dll"

setup="dist/oriel-$version-windows-x64-setup.exe"
portable="dist/oriel-$version-windows-x64-portable.zip"
rm -f "$setup" "$portable"
makensis -V2 -INPUTCHARSET UTF8 \
  "-DVERSION=$version" "-DPAYLOAD=$PWD/$stage" "-DREPO=$PWD" "-DOUTFILE=$PWD/$setup" \
  packaging/windows/installer.nsi
# Windows runs programs from a Samba share only if they are executable here.
chmod 755 "$setup"
(cd target/package && zip -qr "../../$portable" oriel)
(cd dist && sha256sum "$(basename "$setup")" "$(basename "$portable")" > SHA256SUMS.txt)
ls -l "$setup" "$portable"
