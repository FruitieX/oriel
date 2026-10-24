#!/usr/bin/env bash
# A release APK signed with the oriel release key, which lives in 1Password
# (Agents vault: "oriel-release.jks (Android release keystore)" and "oriel
# Android release keystore password"), fetched with op-agent for the build
# only. Run inside `nix develop .#android`; extra arguments go to
# build-android.sh (e.g. --install). With ORIEL_ANDROID_KEYSTORE and
# ORIEL_ANDROID_KEYSTORE_PASS already set, 1Password is not asked for them.
#
# Push needs the Firebase config: android/app/google-services.json or
# ORIEL_GOOGLE_SERVICES_JSON; without either it comes from 1Password ("oriel
# firebase google-services.json").
#
# Items are fetched by ID: the service account is rate limited, and a
# lookup by title costs extra requests.
set -euo pipefail
cd "$(dirname "$0")/.."
keystore_item=agwnd2s6eapvnq7w5gtgre7oba
password_item=dxxufacaiztnf4fqjnswe5nb4m
firebase_item=h4wfcygx4dg63vlnn47cii4tcy
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
chmod 700 "$dir"
if [[ -z ${ORIEL_ANDROID_KEYSTORE:-} || -z ${ORIEL_ANDROID_KEYSTORE_PASS:-} ]]; then
  op-agent document get "$keystore_item" --vault Agents --out-file "$dir/release.jks" --force >/dev/null
  ORIEL_ANDROID_KEYSTORE_PASS=$(op-agent read "op://Agents/$password_item/password")
  ORIEL_ANDROID_KEYSTORE="$dir/release.jks"
fi
if [[ -z ${ORIEL_GOOGLE_SERVICES_JSON:-} && ! -f android/app/google-services.json ]]; then
  op-agent document get "$firebase_item" --vault Agents --out-file "$dir/google-services.json" --force >/dev/null
  export ORIEL_GOOGLE_SERVICES_JSON="$dir/google-services.json"
fi
export ORIEL_ANDROID_KEYSTORE ORIEL_ANDROID_KEYSTORE_PASS
export ORIEL_ANDROID_KEYSTORE_ALIAS=oriel
scripts/build-android.sh --release "$@"
