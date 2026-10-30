# License metadata for libraries a package copies out of the Nix store
# (scripts/third-party-notices.sh): a JSON object from each store path of
# `roots` and of their build inputs, transitively, to its package's name,
# version, licenses, homepage and source. Nothing in it is built (the paths
# carry no string context) except the `texts` below.
{
  lib,
  writeText,
  fetchurl,
  roots,
}:
let
  # License texts for packages that install none and whose source has none
  # at the top: files (libc++ on macOS comes from the 600 MB macOS SDK), or
  # paths in the source.
  extraTexts = {
    libcxx = [
      (fetchurl {
        name = "libcxx-LICENSE.TXT";
        url = "https://raw.githubusercontent.com/llvm/llvm-project/llvmorg-21.1.6/libcxx/LICENSE.TXT";
        hash = "sha256-U53Xrthuik8Sy90ObFDBicfXSEfk/sxkzixu46Ado4s=";
      })
    ];
  };
  sourceTexts = {
    # Apple's (GNU libiconv has a COPYING).
    libiconv = [ "libiconv.txt" ];
    openmp = [ "openmp/LICENSE.TXT" ];
    mingw_w64-pthreads = [ "mingw-w64-libraries/winpthreads/COPYING" ];
  };
  evaluates = x: (builtins.tryEval (builtins.deepSeq x x)).success;
  inputsOf =
    d:
    lib.filter (x: lib.isDerivation x && evaluates x.outPath) (
      lib.flatten [
        (d.buildInputs or [ ])
        (d.propagatedBuildInputs or [ ])
      ]
    );
  closure = builtins.genericClosure {
    startSet = map (d: {
      key = d.outPath;
      inherit d;
    }) roots;
    operator =
      { d, ... }:
      map (x: {
        key = x.outPath;
        d = x;
      }) (inputsOf d);
  };
  plain = x: builtins.unsafeDiscardStringContext (toString x);
  orNull = x: if evaluates x then x else null;
  orEmpty = x: if evaluates x then x else [ ];
  license =
    l:
    if builtins.isAttrs l then
      {
        spdx = l.spdxId or null;
        name = l.fullName or l.shortName or null;
        url = l.url or null;
      }
    else
      {
        spdx = null;
        name = toString l;
        url = null;
      };
  info = d: rec {
    name = d.pname or (lib.getName d);
    version = d.version or (lib.getVersion d);
    licenses = map license (orEmpty (lib.toList (d.meta.license or [ ])));
    homepage = orNull (lib.head (lib.toList (d.meta.homepage or [ null ])));
    src = orNull (if d ? src && d.src != null then plain d.src else null);
    srcUrls = map plain (orEmpty (lib.toList (d.src.urls or d.src.url or [ ])));
    outputs = map (o: plain d.${o}.outPath) (d.outputs or [ "out" ]);
    texts = map toString (extraTexts.${name} or [ ]);
    srcTexts = sourceTexts.${name} or [ ];
  };
  entries = lib.concatMap (
    { d, ... }:
    let
      i = info d;
    in
    map (path: lib.nameValuePair path i) i.outputs
  ) closure;
in
writeText "third-party-meta.json" (builtins.toJSON (lib.listToAttrs entries))
