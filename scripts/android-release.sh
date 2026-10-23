#!/usr/bin/env bash
# A release APK signed with the oriel release key, which lives in 1Password
# (Agents vault: "oriel-release.jks (Android release keystore)" and "oriel
# Android release keystore password"), fetched with op-agent for the build
# only. Run inside `nix develop .#android`; extra arguments go to
# build-android.sh (e.g. --install).
set -euo pipefail
cd "$(dirname "$0")/.."
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
chmod 700 "$dir"
op-agent document get "oriel-release.jks (Android release keystore)" --vault Agents \
  --out-file "$dir/release.jks" --force >/dev/null
ORIEL_ANDROID_KEYSTORE_PASS=$(op-agent read "op://Agents/oriel Android release keystore password/password")
export ORIEL_ANDROID_KEYSTORE="$dir/release.jks" ORIEL_ANDROID_KEYSTORE_PASS
export ORIEL_ANDROID_KEYSTORE_ALIAS=oriel
scripts/build-android.sh --release "$@"
