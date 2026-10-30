#!/usr/bin/env bash
# License notices for the Rust code compiled into one of oriel's programs:
#
#   scripts/rust-notices.sh PACKAGE OUTPUT TARGET...
#
# Writes OUTPUT (THIRD-PARTY-NOTICES-RUST.txt where packages ship it) for
# the crates in PACKAGE's dependency graph on the TARGETs (build and dev
# dependencies left out): cargo-about (about.toml, template
# packaging/notices/rust.hbs) lists them by license, each with its text,
# and fails if one is under a license about.toml does not accept. The C
# code that build scripts compile in (vendored libraries) follows, for the
# crates in the graph. Needs cargo-about and jq (both in the dev shells).
set -euo pipefail

package=$1 output=$(realpath -m "$2")
shift 2
cd "$(dirname "$0")/.."

args=(--frozen --fail -m "crates/$package/Cargo.toml")
for target in "$@"; do args+=(--target "$target"); done
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# cargo-about reads the crates offline; fetch those of other targets first.
cargo fetch --locked --quiet
cargo-about generate "${args[@]}" packaging/notices/rust.hbs -o "$work/crates.txt"
cargo-about generate "${args[@]}" --format json -o "$work/crates.json"

# The source directory of a crate in the graph, empty if it is not in it.
crate_dir() {
  jq -r --arg name "$1" '[.crates[] | select(.package.name == $name)][0].package.manifest_path // empty' \
    "$work/crates.json" | xargs -r dirname
}
crate_version() {
  jq -r --arg name "$1" '[.crates[] | select(.package.name == $name)][0].package.version // empty' \
    "$work/crates.json"
}

# A section for C code: TITLE, LICENSE, then lines of notes; the license
# texts are read from the files in $texts.
texts=()
section() {
  local title=$1 license=$2 file
  shift 2
  echo "================================================================================"
  echo "$title"
  echo
  echo "License: $license"
  for line in "$@"; do echo "$line"; done
  for file in "${texts[@]}"; do
    echo
    echo "--- $(basename "$file")"
    echo
    cat "$file"
  done
  echo
  texts=()
}

{
  echo "Third-party notices for the Rust code in $package ($*)"
  echo
  cat "$work/crates.txt"

  echo "================================================================================"
  echo "The Rust standard library (std, core, alloc and their dependencies)"
  echo
  echo "License: MIT OR Apache-2.0 (texts above); https://github.com/rust-lang/rust"
  echo
  echo "Libraries in C, C++ or assembly that the crates above compile in follow."
  echo

  if dir=$(crate_dir opusic-sys) && [ -n "$dir" ]; then
    texts=("$dir/opus/COPYING")
    section "libopus $(sed -n 's/^PACKAGE_VERSION="\(.*\)"/\1/p' "$dir/opus/package_version"), the Opus audio codec" \
      BSD-3-Clause \
      "Compiled in by opusic-sys $(crate_version opusic-sys) from the sources it bundles." \
      "Homepage: https://opus-codec.org"
  fi
  if dir=$(crate_dir ring) && [ -n "$dir" ]; then
    texts=("$dir/LICENSE" "$dir/LICENSE-BoringSSL" "$dir/LICENSE-other-bits")
    section "C and assembly code in ring $(crate_version ring), much of it from BoringSSL and OpenSSL" \
      "ISC AND Apache-2.0 (each file names its license)" \
      "Source: https://github.com/briansmith/ring"
  fi
  if dir=$(crate_dir libsqlite3-sys) && [ -n "$dir" ]; then
    section "SQLite $(sed -n 's/^#define SQLITE_VERSION *"\(.*\)"/\1/p' "$dir/sqlite3/sqlite3.h")" \
      "public domain (https://sqlite.org/copyright.html)" \
      "Compiled in by libsqlite3-sys $(crate_version libsqlite3-sys) from the amalgamation it bundles."
  fi
  if dir=$(crate_dir libspa-sys) && [ -n "$dir" ]; then
    texts=(packaging/notices/pipewire-COPYING.txt)
    section "PipeWire's SPA headers (their inline functions)" MIT \
      "Compiled into the C wrappers of libspa-sys $(crate_version libspa-sys)." \
      "Homepage: https://pipewire.org"
  fi
  if dir=$(crate_dir android-activity) && [ -n "$dir" ]; then
    texts=("$dir/LICENSE-APACHE")
    section "Android native app glue (android_native_app_glue.c)" Apache-2.0 \
      "Copyright (C) 2021 The Android Open Source Project." \
      "From the Android Game Development Kit, compiled in by android-activity $(crate_version android-activity)." \
      "It links the Android NDK's LLVM libc++ and libc++abi statically" \
      "(Apache-2.0 WITH LLVM-exception, https://github.com/llvm/llvm-project)."
  fi
} > "$output"
echo "wrote $output" >&2
