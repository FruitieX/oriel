#!/usr/bin/env bash
# The Android app: the Rust library (arm64 phones, x86_64 emulators), the
# launcher icon and the license notices (assets/licenses) into target/android, then the Gradle project in
# android/ (Kotlin) builds the APK; it lands in dist/. Run inside
# `nix develop .#android`.
#
#   scripts/build-android.sh [--release] [--abi arm64-v8a|x86_64] [--install]
#
# Signs with ORIEL_ANDROID_KEYSTORE (+ _PASS, _ALIAS) when set (see
# android-release.sh), else with the debug key (~/.android/debug.keystore).
# Push wake-ups need the Firebase config: ORIEL_GOOGLE_SERVICES_JSON, or
# android/app/google-services.json.
set -euo pipefail
cd "$(dirname "$0")/.."

profile=debug
abis=(arm64-v8a x86_64)
install=false
while [ $# -gt 0 ]; do
  case "$1" in
    --release) profile=release ;;
    --abi) abis=("$2"); shift ;;
    --install) install=true ;;
    *) echo "unknown option $1" >&2; exit 1 ;;
  esac
  shift
done
: "${ANDROID_HOME:?run inside nix develop .#android}"

out=target/android
rm -rf "$out/jniLibs" "$out/icons" "$out/assets"
mkdir -p "$out/icons" "$out/assets/licenses" dist

# Rust
cargo_flags=(-p oriel-android)
[ "$profile" = release ] && cargo_flags+=(--release)
targets=()
for abi in "${abis[@]}"; do targets+=(-t "$abi"); done
cargo ndk -P 28 "${targets[@]}" -o "$out/jniLibs" build "${cargo_flags[@]}"

# Licenses: oriel's, and the Rust crates' (and the C code they compile in).
cp LICENSE-MIT LICENSE-APACHE "$out/assets/licenses/"
rust_targets=()
for abi in "${abis[@]}"; do
  case "$abi" in
    arm64-v8a) rust_targets+=(aarch64-linux-android) ;;
    x86_64) rust_targets+=(x86_64-linux-android) ;;
    *) echo "unknown ABI $abi" >&2; exit 1 ;;
  esac
done
./scripts/rust-notices.sh oriel-android "$out/assets/licenses/THIRD-PARTY-NOTICES-RUST.txt" "${rust_targets[@]}"

# The launcher icon in a few densities.
for d in mdpi:48 hdpi:72 xhdpi:96 xxhdpi:144 xxxhdpi:192; do
  mkdir -p "$out/icons/mipmap-${d%%:*}"
  resvg -w "${d##*:}" -h "${d##*:}" assets/oriel.svg "$out/icons/mipmap-${d%%:*}/ic_launcher.png"
done

version=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml)
export ORIEL_VERSION_NAME="$version"
ORIEL_VERSION_CODE=$(git rev-list --count HEAD)
export ORIEL_VERSION_CODE
task=$([ "$profile" = release ] && echo assembleRelease || echo assembleDebug)
(cd android && ./gradlew --quiet --console=plain "$task")
apk="dist/oriel-$version-android.apk"
cp "android/app/build/outputs/apk/$profile/app-$profile.apk" "$apk"
ls -l "$apk"

if $install; then
  "$ANDROID_HOME/platform-tools/adb" install -r "$apk"
  "$ANDROID_HOME/platform-tools/adb" shell am start -n org.fruitiex.oriel/.OrielActivity
fi
