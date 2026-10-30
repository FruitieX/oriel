#!/usr/bin/env bash
# License notices for libraries a package copies out of the Nix store:
#
#   scripts/third-party-notices.sh META DEST LIBRARY...
#
# META is `nix build .#third-party-meta` (store path -> package name,
# version, licenses, homepage and source). For each package a LIBRARY comes
# from, this appends a section to DEST/THIRD-PARTY-NOTICES.txt and copies the
# package's license files to DEST/<name>-<version>/: the texts the metadata
# names and those in any of its outputs that are in the store, else those in
# its source (fetched from the binary cache if needed): at its top and the
# paths the metadata names. Runs on Linux too.
set -euo pipefail

meta=$1 dest=$2
shift 2
notices=$dest/THIRD-PARTY-NOTICES.txt
mkdir -p "$dest"

license_name='^(licen[cs]e|copying|copyright|notice|unlicense)([._-].*)?$'

# License files in a store directory (an output, or an unpacked source):
# those at the top and in share/doc, and everything in share/licenses.
licenses_in() {
  local dir=$1
  {
    find "$dir" -maxdepth 1 -type f
    if [ -d "$dir/share/doc" ]; then find "$dir/share/doc" -maxdepth 3 -type f; fi
  } | while read -r file; do
    if [[ $(basename "$file" | tr '[:upper:]' '[:lower:]') =~ $license_name ]]; then
      echo "$file"
    fi
  done
  if [ -d "$dir/share/licenses" ]; then find "$dir/share/licenses" -type f; fi
}

# Copies a file (or stdin) into a package's license directory as NAME,
# keeping names unique.
keep() {
  local dir=$1 name=$2 source=${3:-/dev/stdin} target n=2
  target=$dir/$name
  while [ -e "$target" ]; do
    target=$dir/$n-$name
    n=$((n + 1))
  done
  cat "$source" > "$target"
}

# The package's license files: from its outputs, else from its source.
collect() {
  local dir=$1 src=$2 found=0 file
  shift 2
  for file in "${texts[@]}"; do
    keep "$dir" "$(basename "$file" | cut -d- -f2-)" "$file"
    found=1
  done
  for out in "$@"; do
    [ -d "$out" ] || continue
    while read -r file; do
      keep "$dir" "$(basename "$file")" "$file"
      found=1
    done < <(licenses_in "$out")
  done
  if ((found)) || [ -z "$src" ]; then return; fi
  [ -e "$src" ] || nix-store --realise "$src" > /dev/null 2>&1 || return 0
  # License files at the top of the source, and those the metadata names.
  if [ -d "$src" ]; then
    while read -r file; do
      keep "$dir" "$(basename "$file")" "$file"
    done < <(licenses_in "$src")
    for file in "${src_texts[@]}"; do
      if [ -f "$src/$file" ]; then keep "$dir" "${file//\//-}" "$src/$file"; fi
    done
  elif tar -tf "$src" > /dev/null 2>&1; then
    tar -tf "$src" | while read -r member; do
      local name=${member#*/}
      if [ -z "$name" ] || [[ $name == */ ]]; then continue; fi
      if [[ $name != */* && $(tr '[:upper:]' '[:lower:]' <<< "$name") =~ $license_name ]] ||
        printf '%s\n' "${src_texts[@]}" | grep -qxF -- "$name"; then
        tar -xOf "$src" "$member" | keep "$dir" "${name//\//-}"
      fi
    done
  fi
}

# Group the libraries by package (a package's outputs share one entry).
declare -A files=() order=()
packages=()
for library in "$@"; do
  path=$(cut -d/ -f1-4 <<< "$library")
  key=$(jq -r --arg p "$path" '.[$p] | if . then "\(.name)-\(.version)" else empty end' "$meta")
  if [ -z "$key" ]; then
    # Not in the metadata: name it after its store path.
    key=$(basename "$path" | cut -d- -f2-)
  fi
  if [ -z "${order[$key]:-}" ]; then
    order[$key]=$path
    packages+=("$key")
  fi
  files[$key]+="${files[$key]:+, }$(basename "$library")"
done

for key in "${packages[@]}"; do
  path=${order[$key]}
  dir=$dest/$key
  rm -rf "$dir"
  mkdir -p "$dir"
  info=$(jq -c --arg p "$path" '.[$p] // {}' "$meta")
  field() { jq -r "$1 // empty" <<< "$info"; }
  mapfile -t texts < <(field '.texts[]?')
  mapfile -t src_texts < <(field '.srcTexts[]?')
  mapfile -t outputs < <(field '.outputs[]?')
  ((${#outputs[@]})) || outputs=("$path")
  collect "$dir" "$(field .src)" "${outputs[@]}"
  {
    echo "$key"
    echo "  Files: ${files[$key]}"
    licenses=$(field '[.licenses[]? | .spdx // .name] | join(", ") | select(. != "")')
    echo "  License: ${licenses:-not recorded in Nixpkgs}"
    if [ -n "$(field .homepage)" ]; then echo "  Homepage: $(field .homepage)"; fi
    while read -r url; do echo "  Source: $url"; done < <(field '.srcUrls[]?')
    if [ -n "$(ls -A "$dir")" ]; then
      echo "  License texts: $key/ ($(cd "$dir" && echo *))"
    else
      rmdir "$dir"
      while read -r url; do echo "  License text: $url"; done < <(field '.licenses[]?.url')
      echo "missing license text: $key" >&2
    fi
    echo
  } >> "$notices"
done
