{
  description = "oriel: low-latency remote desktop host and client";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
    }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs {
        inherit system;
        overlays = [ rust-overlay.overlays.default ];
      };
      lib = pkgs.lib;
      mingw = pkgs.pkgsCross.mingwW64;

      rust = pkgs.rust-bin.stable.latest.default.override {
        extensions = [
          "rust-src"
          "rust-analyzer"
          "clippy"
          "rustfmt"
        ];
        targets = [ "x86_64-pc-windows-gnu" ];
      };

      # Prebuilt LGPL FFmpeg for the Windows host (NVENC, AMF and QSV/VPL
      # encoders, D3D11VA). BtbN prunes dated autobuilds after a while; a copy
      # is attached to the FruitieX/oriel release `deps-ffmpeg-8.1.3`
      # (`gh release download`, then `nix store add-file`) if this URL dies.
      ffmpeg-win64 = pkgs.fetchzip {
        url = "https://github.com/BtbN/FFmpeg-Builds/releases/download/autobuild-2026-10-21-13-06/ffmpeg-n8.1.3-20-gc8de997cb1-win64-lgpl-shared-8.1.zip";
        hash = "sha256-Y0aC2ExTMwoIILKwvAFPNuhRvw7Mf5n8iBCvKDjBZsc=";
      };

      # Libraries winit/wgpu/cpal dlopen at runtime on Linux.
      runtimeLibs = with pkgs; [
        wayland
        libxkbcommon
        vulkan-loader
        libGL
        libx11
        libxcursor
        libxi
        libxrandr
        alsa-lib
        pipewire
      ];
    in
    {
      devShells.${system}.default = pkgs.mkShell {
        nativeBuildInputs = with pkgs; [
          rust
          pkg-config
          clang
          cmake
          ninja
          glslang
          wineWow64Packages.stable
          vulkan-tools
        ];
        buildInputs =
          with pkgs;
          [
            ffmpeg_8.dev
            ffmpeg_8.lib
            opus
            libva
            pipewire.dev
            vulkan-headers
          ]
          ++ runtimeLibs;

        LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
        LD_LIBRARY_PATH = lib.makeLibraryPath runtimeLibs;

        # Cross-compiling the Windows build (see scripts/build-windows.sh).
        # The mingw compiler is not on nativeBuildInputs: its setup hook would
        # take over CC for native builds.
        CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER = "${mingw.stdenv.cc}/bin/x86_64-w64-mingw32-cc";
        CC_x86_64_pc_windows_gnu = "${mingw.stdenv.cc}/bin/x86_64-w64-mingw32-cc";
        CXX_x86_64_pc_windows_gnu = "${mingw.stdenv.cc}/bin/x86_64-w64-mingw32-c++";
        AR_x86_64_pc_windows_gnu = "${mingw.stdenv.cc.bintools.bintools}/bin/x86_64-w64-mingw32-ar";
        CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUSTFLAGS = "-L native=${mingw.windows.pthreads}/lib";
        ORIEL_FFMPEG_WIN64 = ffmpeg-win64;
        ORIEL_MINGW_PTHREADS = mingw.windows.pthreads;
        ORIEL_MINGW_CC = mingw.stdenv.cc;
        ORIEL_WINE = "${pkgs.wineWow64Packages.stable}/bin/wine";

        shellHook = ''
          # Prefixed mingw tools (dlltool for windows-rs import libraries).
          export PATH="$PATH:${mingw.stdenv.cc}/bin:${mingw.stdenv.cc.bintools.bintools}/bin"
        '';
      };

      formatter.${system} = pkgs.nixfmt-rfc-style;
    };
}
