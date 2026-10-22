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
    rev = "53090429539bf3404250d18fe58db971f0907c04";
    hash = "sha256-9ro0Tsf22ZQEG9CID5/7AbpZmrzZb0DyBlr/rnvvfHE=";
  };
  volk = fetchFromGitHub {
    owner = "zeux";
    repo = "volk";
    rev = "47cddf7ed97b94118a08aacb548a411188e016cc";
    hash = "sha256-46z0o6AnID2RGZn0jJTmEwZW/S0kP5TFb2bR5PfqEuw=";
  };
  vulkanHeaders = fetchFromGitHub {
    owner = "KhronosGroup";
    repo = "Vulkan-Headers";
    rev = "6802bb4733b63ed5efd3adb308a6c885ef180ea1";
    hash = "sha256-E/SBlMW1oUeUT0MtWzQp0k+OSZLc9ho7g+1oTd7z7+Y=";
  };
in
stdenv.mkDerivation {
  pname = "pyrowave";
  version = "1.0.0-unstable-2026-10-17";

  src = fetchFromGitHub {
    owner = "Themaister";
    repo = "pyrowave";
    rev = "21f099d24df0ab0e4c10c3288c924b6b1717e581";
    hash = "sha256-Is/hlS8WifU+CimRrSZ1DvYRL81TYMVZfrRdKxfN2OQ=";
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

  ninjaFlags = [ "pyrowave-shared" ];

  installPhase = ''
    runHook preInstall
    mkdir -p $out/lib $out/bin $out/include/pyrowave
    cp ../pyrowave.h $out/include/pyrowave/
    cp -P libpyrowave-shared.so* $out/lib/ 2>/dev/null || true
    cp -P libpyrowave-shared*.dylib $out/lib/ 2>/dev/null || true
    cp *.dll $out/bin/ 2>/dev/null || true
    runHook postInstall
  '';

  meta = {
    description = "Ultra low-latency intra-only wavelet video codec (C API)";
    homepage = "https://github.com/Themaister/pyrowave";
    license = lib.licenses.mit;
  };
}
