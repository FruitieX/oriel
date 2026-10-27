#!/usr/bin/env bash
# macOS package: a release build as dist/oriel.app (with the icon and the
# FFmpeg libraries from the Nix store inside, ad-hoc signed) and a ZIP of
# it. Run inside `nix develop`. Drag the app to /Applications to install.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build --release -p oriel
version=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml)
[ -n "$version" ] || { echo "no version in Cargo.toml" >&2; exit 1; }

app=dist/oriel.app
contents=$app/Contents
rm -rf "$app"
mkdir -p "$contents/MacOS" "$contents/Frameworks" "$contents/Resources"
install -m 755 target/release/oriel "$contents/MacOS/oriel"
# PyroWave (Metal port), which oriel looks for next to itself.
if [ -n "${ORIEL_PYROWAVE_LIB:-}" ]; then
  install -m 644 "$ORIEL_PYROWAVE_LIB" "$contents/MacOS/libpyrowave-metal.1.dylib"
else
  echo "ORIEL_PYROWAVE_LIB unset: the app will not offer PyroWave" >&2
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# The icon, from assets/oriel.svg.
iconset=$work/oriel.iconset
mkdir "$iconset"
for size in 16 32 128 256 512; do
  resvg -w "$size" -h "$size" assets/oriel.svg "$iconset/icon_${size}x${size}.png"
  resvg -w $((size * 2)) -h $((size * 2)) assets/oriel.svg "$iconset/icon_${size}x${size}@2x.png"
done
iconutil -c icns -o "$contents/Resources/oriel.icns" "$iconset"

cat > "$contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleExecutable</key><string>oriel</string>
  <key>CFBundleIdentifier</key><string>org.fruitiex.oriel</string>
  <key>CFBundleName</key><string>oriel</string>
  <key>CFBundleDisplayName</key><string>oriel</string>
  <key>CFBundleIconFile</key><string>oriel</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>14.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSMicrophoneUsageDescription</key>
  <string>oriel sends your microphone to the remote computer when you turn that on.</string>
</dict>
</plist>
PLIST

# The libraries the binary loads from the Nix store, and theirs, go to
# Frameworks. Two packages can ship a library under the same name (GNU and
# Apple's libiconv): those are prefixed with their package's name.
nix_deps() { otool -L "$1" | tail -n +2 | awk '{print $1}' | grep '^/nix/store/' || true; }
declare -A bundled=() taken=()
queue=("$contents/MacOS/oriel")
while ((${#queue[@]})); do
  file=${queue[0]}
  queue=("${queue[@]:1}")
  for dep in $(nix_deps "$file"); do
    [ -n "${bundled[$dep]:-}" ] && continue
    name=$(basename "$dep")
    if [ -n "${taken[$name]:-}" ]; then
      package=$(echo "$dep" | cut -d/ -f4 | cut -d- -f2-)
      name="$package-$name"
    fi
    taken[$name]=1
    bundled[$dep]=$name
    install -m 644 "$(realpath "$dep")" "$contents/Frameworks/$name"
    queue+=("$dep")
  done
done

# Point every load at @rpath and drop the Nix store run paths.
relink() {
  local file=$1 args=()
  for dep in $(nix_deps "$file"); do
    args+=(-change "$dep" "@rpath/${bundled[$dep]}")
  done
  for rpath in $(otool -l "$file" | awk '/LC_RPATH/ {getline; getline; print $2}' | grep '^/nix/store/' || true); do
    args+=(-delete_rpath "$rpath")
  done
  if ((${#args[@]})); then install_name_tool "${args[@]}" "$file" 2>/dev/null; fi
}
for dep in "${!bundled[@]}"; do
  lib=$contents/Frameworks/${bundled[$dep]}
  install_name_tool -id "@rpath/${bundled[$dep]}" "$lib" 2>/dev/null
  relink "$lib"
  install_name_tool -add_rpath @loader_path "$lib" 2>/dev/null
done
relink "$contents/MacOS/oriel"
install_name_tool -add_rpath @executable_path/../Frameworks "$contents/MacOS/oriel" 2>/dev/null

if otool -L "$contents/MacOS/oriel" "$contents"/Frameworks/* | grep -q /nix/store; then
  echo "the bundle still loads from /nix/store" >&2
  exit 1
fi

# Ad-hoc signature: runs on this Mac (and others after "Open" once); not
# notarized.
codesign --force --sign - "$contents"/Frameworks/* "$contents"/MacOS/*.dylib "$app" 2>&1 |
  { grep -v 'replacing existing signature' || true; } >&2

zip="dist/oriel-$version-macos-$(uname -m).zip"
rm -f "$zip"
ditto -c -k --keepParent "$app" "$zip"
echo "${#bundled[@]} libraries bundled"
ls -ld "$app" "$zip"
