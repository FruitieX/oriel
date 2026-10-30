{
  description = "oriel: low-latency remote desktop host and client";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    # The Android SDK and NDK for the Android client (`nix develop .#android`).
    android-nixpkgs = {
      url = "github:tadfisher/android-nixpkgs";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
      android-nixpkgs,
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
        "x86_64-darwin"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f system);
      pkgsFor =
        system:
        import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };

      # Libraries winit/wgpu/cpal dlopen at runtime on Linux.
      linuxRuntimeLibs =
        pkgs: with pkgs; [
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
          libva
          # gilrs (gamepads)
          systemd
        ];
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
          lib = pkgs.lib;
          pyrowave = pkgs.callPackage ./nix/pyrowave.nix { };
        in
        rec {
          inherit pyrowave;
          # The optional hub service (crates/oriel-hub), and its container
          # for the homelab (published by .github/workflows/hub.yml).
          hub = pkgs.rustPlatform.buildRustPackage {
            pname = "oriel-hub";
            version = "0.1.0";
            src = lib.cleanSource ./.;
            cargoLock.lockFile = ./Cargo.lock;
            cargoBuildFlags = [
              "-p"
              "oriel-hub"
            ];
            cargoTestFlags = [
              "-p"
              "oriel-hub"
            ];
            meta.mainProgram = "oriel-hub";
          };
          hub-container = pkgs.dockerTools.buildLayeredImage {
            name = "ghcr.io/fruitiex/oriel-hub";
            tag = "main";
            contents = [ pkgs.cacert ];
            extraCommands = "mkdir -p data tmp; chmod 1777 tmp";
            fakeRootCommands = "chown 10001:10001 data";
            config = {
              User = "10001:10001";
              Entrypoint = [ "${hub}/bin/oriel-hub" ];
              Env = [
                "ORIEL_HUB_BIND=0.0.0.0:8080"
                "ORIEL_HUB_DATA_DIR=/data"
                "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
              ];
              ExposedPorts = {
                "8080/tcp" = { };
                "48700/udp" = { };
              };
              Volumes."/data" = { };
            };
          };
          default = pkgs.rustPlatform.buildRustPackage {
            pname = "oriel";
            version = "0.1.0";
            src = lib.cleanSource ./.;
            cargoLock.lockFile = ./Cargo.lock;
            cargoBuildFlags = [
              "-p"
              "oriel"
            ];
            # Shown in Settings → App (the source has no .git).
            ORIEL_GIT_SHA = self.shortRev or self.dirtyShortRev or "unknown";
            ORIEL_BUILD_DATE =
              let
                d = self.lastModifiedDate or "19700101000000";
              in
              "${builtins.substring 0 4 d}-${builtins.substring 4 2 d}-${builtins.substring 6 2 d} ${builtins.substring 8 2 d}:${builtins.substring 10 2 d} UTC";
            doCheck = false;
            nativeBuildInputs = with pkgs; [
              pkg-config
              cmake
              rustPlatform.bindgenHook
              makeWrapper
              copyDesktopItems
            ];
            desktopItems = [
              (pkgs.makeDesktopItem {
                name = "oriel";
                desktopName = "oriel";
                genericName = "Remote desktop";
                comment = "Connect to other computers or share this one";
                exec = "oriel";
                icon = "oriel";
                startupWMClass = "oriel";
                categories = [
                  "Network"
                  "RemoteAccess"
                ];
              })
            ];
            postInstall = ''
              install -Dm644 assets/oriel.svg $out/share/icons/hicolor/scalable/apps/oriel.svg
            '';
            buildInputs = [
              pkgs.ffmpeg_8.dev
              pkgs.opus
            ]
            ++ lib.optionals pkgs.stdenv.hostPlatform.isLinux (
              [
                pkgs.pipewire.dev
                pkgs.systemd.dev
              ]
              ++ linuxRuntimeLibs pkgs
            );
            postFixup = lib.optionalString pkgs.stdenv.hostPlatform.isLinux ''
              wrapProgram $out/bin/oriel \
                --prefix LD_LIBRARY_PATH : ${lib.makeLibraryPath (linuxRuntimeLibs pkgs ++ [ pyrowave ])}
            '';
            meta = {
              description = "Low-latency remote desktop host and client";
              mainProgram = "oriel";
            };
          };
        }
      );

      devShells = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
          lib = pkgs.lib;
          isLinux = pkgs.stdenv.hostPlatform.isLinux;
          # Cross-compiling the Windows host is set up on x86_64 Linux only.
          windowsCross = system == "x86_64-linux";
          mingw = pkgs.pkgsCross.mingwW64;
          pyrowave = pkgs.callPackage ./nix/pyrowave.nix { };

          rust = pkgs.rust-bin.stable.latest.default.override {
            extensions = [
              "rust-src"
              "rust-analyzer"
              "clippy"
              "rustfmt"
            ];
            targets = lib.optional windowsCross "x86_64-pc-windows-gnu";
          };

          # Prebuilt LGPL FFmpeg for the Windows host (NVENC, AMF and QSV/VPL
          # encoders, D3D11VA). BtbN prunes dated autobuilds after a while; a
          # copy is attached to the FruitieX/oriel release `deps-ffmpeg-8.1.3`
          # (`gh release download`, then `nix store add-file`) if this URL dies.
          ffmpeg-win64 = pkgs.fetchzip {
            url = "https://github.com/BtbN/FFmpeg-Builds/releases/download/autobuild-2026-10-21-13-06/ffmpeg-n8.1.3-20-gc8de997cb1-win64-lgpl-shared-8.1.zip";
            hash = "sha256-Y0aC2ExTMwoIILKwvAFPNuhRvw7Mf5n8iBCvKDjBZsc=";
          };

          androidShell =
            { emulator }:
            let
              androidSdk = android-nixpkgs.sdk.${system} (
                sdk:
                with sdk;
                [
                  cmdline-tools-latest
                  build-tools-36-0-0
                  platform-tools
                  platforms-android-35
                  ndk-30-0-16248370
                ]
                ++ lib.optionals emulator [
                  sdk.emulator
                  system-images-android-35-google-apis-x86-64
                ]
              );
              rustAndroid = pkgs.rust-bin.stable.latest.default.override {
                extensions = [
                  "rust-src"
                  "clippy"
                  "rustfmt"
                ];
                targets = [
                  "aarch64-linux-android"
                  "x86_64-linux-android"
                ];
              };
            in
            pkgs.mkShell {
              nativeBuildInputs = with pkgs; [
                rustAndroid
                cargo-ndk
                jdk17
                cmake
                ninja
                pkg-config
                zip
                unzip
                # The launcher icon from assets/oriel.svg.
                resvg
              ];
              ANDROID_HOME = "${androidSdk}/share/android-sdk";
              ANDROID_SDK_ROOT = "${androidSdk}/share/android-sdk";
              ANDROID_NDK_HOME = "${androidSdk}/share/android-sdk/ndk/30.0.16248370";
              JAVA_HOME = "${pkgs.jdk17}";
              LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
            };
        in
        {
          # The Android client: `scripts/build-android.sh` builds the APK.
          android = androidShell { emulator = true; };
          # The same without the emulator, for CI.
          android-ci = androidShell { emulator = false; };

          default = pkgs.mkShell (
            {
              nativeBuildInputs =
                with pkgs;
                [
                  rust
                  # Windows installer (scripts/package-windows.sh)
                  nsis
                  zip
                  pkg-config
                  clang
                  cmake
                  ninja
                  glslang
                ]
                ++ lib.optionals isLinux [ vulkan-tools ]
                # The app icon (scripts/package-macos.sh)
                ++ lib.optionals pkgs.stdenv.hostPlatform.isDarwin [ resvg ]
                ++ lib.optionals windowsCross [ wineWow64Packages.stable ];
              buildInputs =
                with pkgs;
                [
                  ffmpeg_8.dev
                  ffmpeg_8.lib
                  opus
                  vulkan-headers
                ]
                ++ lib.optionals isLinux (
                  [
                    pipewire.dev
                    systemd.dev
                  ]
                  ++ linuxRuntimeLibs pkgs
                );

              LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
            }
            // lib.optionalAttrs isLinux {
              ORIEL_PYROWAVE_LIB = "${pyrowave}/lib/libpyrowave-shared.so.1";
              LD_LIBRARY_PATH = lib.makeLibraryPath (linuxRuntimeLibs pkgs);
            }
            // lib.optionalAttrs pkgs.stdenv.hostPlatform.isDarwin {
              # PyroWave's native Metal port (see nix/pyrowave.nix).
              ORIEL_PYROWAVE_LIB = "${pyrowave}/lib/libpyrowave-metal.1.dylib";
            }
            // lib.optionalAttrs windowsCross {
              # The mingw compiler is not on nativeBuildInputs: its setup hook
              # would take over CC for native builds.
              CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER = "${mingw.stdenv.cc}/bin/x86_64-w64-mingw32-cc";
              CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUSTFLAGS = "-L native=${mingw.windows.pthreads}/lib";
              CC_x86_64_pc_windows_gnu = "${mingw.stdenv.cc}/bin/x86_64-w64-mingw32-cc";
              CXX_x86_64_pc_windows_gnu = "${mingw.stdenv.cc}/bin/x86_64-w64-mingw32-c++";
              AR_x86_64_pc_windows_gnu = "${mingw.stdenv.cc.bintools.bintools}/bin/x86_64-w64-mingw32-ar";
              ORIEL_FFMPEG_WIN64 = ffmpeg-win64;
              ORIEL_MINGW_PTHREADS = mingw.windows.pthreads;
              ORIEL_MINGW_CC = mingw.stdenv.cc;
              ORIEL_WINE = "${pkgs.wineWow64Packages.stable}/bin/wine";
              ORIEL_PYROWAVE_WIN64 = mingw.callPackage ./nix/pyrowave.nix { };

              shellHook = ''
                # Prefixed mingw tools (dlltool for windows-rs import libraries).
                export PATH="$PATH:${mingw.stdenv.cc}/bin:${mingw.stdenv.cc.bintools.bintools}/bin"
              '';
            }
          );
        }
      );

      formatter = forAllSystems (system: (pkgsFor system).nixfmt-rfc-style);
    };
}
