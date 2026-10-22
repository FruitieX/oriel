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
cp packaging/windows/THIRD-PARTY-NOTICES.txt "$stage/licenses/"
cp "$ORIEL_FFMPEG_WIN64/LICENSE.txt" "$stage/licenses/FFmpeg-LICENSE.txt"

setup="dist/oriel-$version-windows-x64-setup.exe"
portable="dist/oriel-$version-windows-x64-portable.zip"
rm -f "$setup" "$portable"
makensis -V2 -INPUTCHARSET UTF8 \
  "-DVERSION=$version" "-DPAYLOAD=$PWD/$stage" "-DREPO=$PWD" "-DOUTFILE=$PWD/$setup" \
  packaging/windows/installer.nsi
(cd target/package && zip -qr "../../$portable" oriel)
(cd dist && sha256sum "$(basename "$setup")" "$(basename "$portable")" > SHA256SUMS.txt)
ls -l "$setup" "$portable"
