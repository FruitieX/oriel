# PyroWave's C API as a shared library (libpyrowave-shared / pyrowave-shared-1.dll).
# Only the slice of Granite that checkout_granite.sh fetches is needed.
{
  lib,
  stdenv,
  fetchFromGitHub,
  cmake,
  ninja,
  windows ? null,
}:
let
  granite = fetchFromGitHub {
    owner = "Themaister";
    repo = "Granite";
    rev = "69f450c62dc3780680b936539a931b5d03e436fb";
    hash = "sha256-9ro0Tsf22ZQEG9CID5/7AbpZmrzZb0DyBlr/rnvvfHE=";
  };
  volk = fetchFromGitHub {
    owner = "zeux";
    repo = "volk";
    rev = "b7764bc7f5880139859a15d1609d2ee0c6251608";
    hash = "sha256-JPvPhETKYQRJ/FeJEx0pTYzqt6Vnm+Rp3cAuB/MJvFM=";
  };
  vulkanHeaders = fetchFromGitHub {
    owner = "KhronosGroup";
    repo = "Vulkan-Headers";
    rev = "9fbc2a782ee3f8a4db64e7d991e23da75e893f44";
    hash = "sha256-Y7w5o+kDsEXN3r0ADreS8WwBZjJvnO5VOSt9WZo0rDk=";
  };
in
stdenv.mkDerivation {
  pname = "pyrowave";
  version = "1.0.0-unstable-2026-10-27";

  src = fetchFromGitHub {
    owner = "Themaister";
    repo = "pyrowave";
    rev = "7fe0e2667cd3076b8b2bf04bcb80bc8d691b800b";
    hash = "sha256-dtIWgg6OiNDAr9+bFtsnGPQOKhIU/bIg0ffNd+LX2tM=";
  };

  postUnpack = ''
    cp -r ${granite} $sourceRoot/Granite
    chmod -R u+w $sourceRoot/Granite
    rm -rf $sourceRoot/Granite/third_party/volk $sourceRoot/Granite/third_party/khronos/vulkan-headers
    cp -r ${volk} $sourceRoot/Granite/third_party/volk
    cp -r ${vulkanHeaders} $sourceRoot/Granite/third_party/khronos/vulkan-headers
    chmod -R u+w $sourceRoot/Granite
  '';

  nativeBuildInputs = [
    cmake
    ninja
  ];

  # Statically linked into the DLL on MinGW.
  buildInputs = lib.optional stdenv.hostPlatform.isWindows windows.pthreads;

  cmakeFlags = [ "-DCMAKE_BUILD_TYPE=Release" ];
  # macOS: the native Metal port (metal/, no Granite or Vulkan), which
  # oriel's macOS client loads instead.
  cmakeDir = if stdenv.hostPlatform.isDarwin then "../metal" else "..";

  ninjaFlags = [ (if stdenv.hostPlatform.isDarwin then "pyrowave-metal" else "pyrowave-shared") ];

  installPhase = ''
    runHook preInstall
    mkdir -p $out/lib $out/bin $out/include/pyrowave
    cp ../pyrowave.h ../metal/pyrowave_metal.h $out/include/pyrowave/
    cp -P libpyrowave-shared.so* $out/lib/ 2>/dev/null || true
    cp -P libpyrowave-metal*.dylib $out/lib/ 2>/dev/null || true
    cp *.dll $out/bin/ 2>/dev/null || true
    install -Dm644 ../LICENSE $out/share/licenses/pyrowave/LICENSE
  ''
  # The Vulkan build compiles in parts of Granite, volk and Vulkan-Headers.
  + lib.optionalString (!stdenv.hostPlatform.isDarwin) ''
    install -Dm644 ../Granite/LICENSE $out/share/licenses/pyrowave/Granite-LICENSE
    install -Dm644 ../Granite/third_party/volk/LICENSE.md $out/share/licenses/pyrowave/volk-LICENSE.md
    install -Dm644 ../Granite/third_party/khronos/vulkan-headers/LICENSE.md \
      $out/share/licenses/pyrowave/Vulkan-Headers-LICENSE.md
    for f in ../Granite/third_party/khronos/vulkan-headers/LICENSES/*; do
      install -Dm644 "$f" "$out/share/licenses/pyrowave/Vulkan-Headers-$(basename "$f")"
    done
  ''
  + ''
    runHook postInstall
  '';

  meta = {
    description = "Ultra low-latency intra-only wavelet video codec (C API)";
    homepage = "https://github.com/Themaister/pyrowave";
    license = lib.licenses.mit;
  };
}
