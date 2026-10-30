# License notices for the Nix packages in a container image (the hub's):
# THIRD-PARTY-NOTICES.txt names each package with its licenses and where its
# exact source and Nixpkgs build recipe (with patches) are, and each
# package's license texts go to a directory named after it, taken from its
# source. The build fails when the closure of `rootPaths` has a store path
# that neither `own` nor an entry of `packages` accounts for: add new
# runtime dependencies here.
{
  lib,
  runCommand,
  closureInfo,
  spdx-license-list-data,
  # What the image holds; their runtime closure is checked.
  rootPaths,
  # Store paths whose licenses are shipped separately (oriel's own).
  own,
  # [{ drv; paths ? [ ]; texts ? [ ]; spdxTexts ? [ ]; note ? null; }]:
  # `drv` (its outputs, and `paths`, store paths built from the same
  # source), `texts` the license files at the top of its source, and
  # `spdxTexts` SPDX license texts for ones the source lacks.
  packages,
  # The Nixpkgs revision the packages come from.
  nixpkgsRev,
}:
let
  closure = closureInfo { inherit rootPaths; };
  plain = x: builtins.unsafeDiscardStringContext (toString x);
  recipe =
    d:
    let
      position = d.meta.position or "";
      m = builtins.match ".*-source/(.*):([0-9]+)" position;
    in
    if m == null then
      "https://github.com/NixOS/nixpkgs/tree/${nixpkgsRev}"
    else
      "https://github.com/NixOS/nixpkgs/blob/${nixpkgsRev}/${lib.head m}#L${lib.last m}";
  entry =
    {
      drv,
      paths ? [ ],
      texts ? [ ],
      spdxTexts ? [ ],
      note ? null,
    }:
    let
      # Some packages (libidn2) are wrappers whose `out` is the real build.
      real = if drv ? src then drv else drv.out;
      src = real.src;
      key = "${drv.pname or (lib.getName drv)}-${drv.version or (lib.getVersion drv)}";
      licenses = map (l: l.spdxId or l.fullName or (toString l)) (lib.toList (drv.meta.license or [ ]));
      hash = src.outputHash or null;
      urls = lib.toList (src.urls or src.url or [ ]);
      info = lib.concatStringsSep "\n" (
        [
          "  License: ${lib.concatStringsSep ", " licenses}"
        ]
        ++ lib.optional (
          drv.meta.homepage or null != null
        ) "  Homepage: ${lib.head (lib.toList drv.meta.homepage)}"
        ++ map (u: "  Source: ${u}") urls
        ++ lib.optional (hash != null) "  Source hash: ${hash}"
        ++ [ "  Nixpkgs recipe and patches: ${recipe real}" ]
        ++ lib.optional (note != null) "  ${note}"
        ++ [ "  License texts: ${key}/" ]
      );
      outputs = map (o: plain drv.${o}.outPath) (drv.outputs or [ "out" ]) ++ map plain paths;
    in
    {
      inherit outputs;
      script = ''
        mkdir -p "$out/${key}"
        ${lib.concatMapStrings (t: ''
          tar -xOf ${src} --wildcards --no-wildcards-match-slash '*/${t}' > "$out/${key}/${t}"
        '') texts}
        ${lib.concatMapStrings (t: ''
          cp ${spdx-license-list-data.text}/text/${t}.txt "$out/${key}/${t}.txt"
        '') spdxTexts}
        {
          echo ${lib.escapeShellArg key}
          echo "  In the image: $(grep -xF ${
            lib.concatMapStringsSep " " (o: "-e ${o}") outputs
          } ${closure}/store-paths | tr '\n' ' ' | sed 's/ $//')"
        } >> "$out/THIRD-PARTY-NOTICES.txt"
        cat >> "$out/THIRD-PARTY-NOTICES.txt" <<'EOF'
        ${info}

        EOF
      '';
    };
  entries = map entry packages;
  known = lib.concatMap (e: e.outputs) entries ++ map plain own;
in
runCommand "closure-notices"
  {
    known = lib.concatStringsSep "\n" known;
    passAsFile = [ "known" ];
  }
  ''
    unknown=$(grep -vxFf "$knownPath" ${closure}/store-paths || true)
    if [ -n "$unknown" ]; then
      echo "store paths in the image without license notices (nix/closure-notices.nix):" >&2
      echo "$unknown" >&2
      exit 1
    fi
    mkdir -p $out
    cat > $out/THIRD-PARTY-NOTICES.txt <<'EOF'
    The image is built with Nix from https://github.com/FruitieX/oriel (its
    flake.lock pins Nixpkgs ${nixpkgsRev}). Besides oriel-hub (its licenses
    and those of the Rust crates in it are LICENSE-MIT, LICENSE-APACHE and
    THIRD-PARTY-NOTICES-RUST.txt) it contains the Nixpkgs packages below.
    Each lists the exact upstream source it is built from (with its hash)
    and the Nixpkgs recipe with the patches applied to it; together they are
    the complete corresponding source. `nix build .#hub-container` in the
    oriel repository at the image's revision rebuilds the image from them.

    EOF
    ${lib.concatMapStrings (e: e.script) entries}
  ''
