#!/usr/bin/env bash
# The Android app: builds the Rust library (arm64 phones, x86_64 emulators),
# the Java glue, and a signed APK in dist/. Run inside `nix develop .#android`.
#
#   scripts/build-android.sh [--release] [--abi arm64-v8a|x86_64] [--install]
#
# Signs with ORIEL_ANDROID_KEYSTORE (+ _PASS, _ALIAS) when set, else with
# the local debug keystore (~/.android/debug.keystore, created if missing).
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
api=28
target_api=35
build_tools="$ANDROID_HOME/build-tools/35.0.0"
android_jar="$ANDROID_HOME/platforms/android-$target_api/android.jar"
strip="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-strip"

out=target/android
rm -rf "$out/apk" "$out/classes" "$out/res"
mkdir -p "$out/apk" "$out/classes" "$out/res" dist

# Rust
cargo_flags=(-p oriel-android)
[ "$profile" = release ] && cargo_flags+=(--release)
targets=()
for abi in "${abis[@]}"; do targets+=(-t "$abi"); done
cargo ndk -P "$api" "${targets[@]}" -o "$out/jniLibs" build "${cargo_flags[@]}"
for abi in "${abis[@]}"; do
  mkdir -p "$out/apk/lib/$abi"
  "$strip" --strip-debug -o "$out/apk/lib/$abi/liboriel_android.so" \
    "$out/jniLibs/$abi/liboriel_android.so"
done
# Only the ABIs built this time.
for dir in "$out"/jniLibs/*; do
  [[ " ${abis[*]} " == *" $(basename "$dir") "* ]] || rm -rf "$dir"
done

# Java glue
javac --release 11 -nowarn -classpath "$android_jar" -d "$out/classes" \
  $(find android/java -name '*.java')
"$build_tools/d8" --min-api "$api" --lib "$android_jar" --output "$out/apk" \
  $(find "$out/classes" -name '*.class')

# Resources: the launcher icon in a few densities.
for d in mdpi:48 hdpi:72 xhdpi:96 xxhdpi:144 xxxhdpi:192; do
  mkdir -p "$out/res/mipmap-${d%%:*}"
  resvg -w "${d##*:}" -h "${d##*:}" assets/oriel.svg "$out/res/mipmap-${d%%:*}/ic_launcher.png"
done
"$build_tools/aapt2" compile --dir "$out/res" -o "$out/res.zip"
version=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml)
code=$(git rev-list --count HEAD)
"$build_tools/aapt2" link -o "$out/base.apk" -I "$android_jar" \
  --manifest android/AndroidManifest.xml "$out/res.zip" \
  --min-sdk-version "$api" --target-sdk-version "$target_api" \
  --version-name "$version" --version-code "$code" \
  $([ "$profile" = debug ] && echo --debug-mode)

# Assemble, align, sign
cp "$out/base.apk" "$out/unsigned.apk"
(cd "$out/apk" && zip -qr ../unsigned.apk classes.dex lib)
"$build_tools/zipalign" -f -P 16 4 "$out/unsigned.apk" "$out/aligned.apk"
apk="dist/oriel-$version-android.apk"
if [ -n "${ORIEL_ANDROID_KEYSTORE:-}" ]; then
  "$build_tools/apksigner" sign --ks "$ORIEL_ANDROID_KEYSTORE" \
    --ks-pass "env:ORIEL_ANDROID_KEYSTORE_PASS" \
    --ks-key-alias "${ORIEL_ANDROID_KEYSTORE_ALIAS:-oriel}" \
    --out "$apk" "$out/aligned.apk"
else
  ks="$HOME/.android/debug.keystore"
  if [ ! -f "$ks" ]; then
    mkdir -p "$(dirname "$ks")"
    keytool -genkeypair -keystore "$ks" -storepass android -keypass android \
      -alias androiddebugkey -keyalg RSA -keysize 2048 -validity 10000 \
      -dname "CN=Android Debug,O=Android,C=US"
  fi
  "$build_tools/apksigner" sign --ks "$ks" --ks-pass pass:android \
    --out "$apk" "$out/aligned.apk"
fi
rm -f "$apk.idsig"
ls -l "$apk"

if $install; then
  "$ANDROID_HOME/platform-tools/adb" install -r "$apk"
  "$ANDROID_HOME/platform-tools/adb" shell am start -n org.fruitiex.oriel/.OrielActivity
fi
