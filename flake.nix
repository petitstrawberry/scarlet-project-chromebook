{
  description = "Scarlet Chromebook bring-up and ChromiumOS debug environment";

  nixConfig = {
    extra-substituters = [ "https://scarlet-rust-toolchain.cachix.org" ];
    extra-trusted-public-keys = [
      "scarlet-rust-toolchain.cachix.org-1:p+coBExi0nNTIvWF/oM9H9/1/GhwFtqGZ2Vs+4pYl6o="
    ];
  };

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    scarlet-rust-toolchain.url = "github:petitstrawberry/scarlet-rust-nix";
    scarlet-sdk = {
      url = "github:petitstrawberry/scarlet-sdk/a071222231e8bbd96c1d54f38d99d1b27ec90805";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.scarlet-rust-toolchain.follows = "scarlet-rust-toolchain";
    };
    vulkan-headers-src = {
      url = "github:KhronosGroup/Vulkan-Headers/c46850864f4661461b0f6cb9922c058ffea4915e";
      flake = false;
    };
    vboot-reference = {
      url = "git+https://chromium.googlesource.com/chromiumos/platform/vboot_reference?rev=c71f57be588c1ca69052e6f7208cc16437db513e";
      flake = false;
    };
  };

  outputs =
    {
      nixpkgs,
      scarlet-rust-toolchain,
      scarlet-sdk,
      vulkan-headers-src,
      vboot-reference,
      ...
    }:
    let
      supportedSystems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs supportedSystems (system: f system);
      mkVbootTools =
        pkgs:
        pkgs.stdenv.mkDerivation {
          pname = "vboot-utils";
          version = "c71f57be588c";
          src = vboot-reference;

          nativeBuildInputs = [
            pkgs.gnumake
            pkgs.gnused
            pkgs.pkg-config
          ];
          buildInputs = [ pkgs.openssl ] ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [
            pkgs.libuuid
            pkgs.libyaml
            pkgs.xz
          ];

          postPatch = ''
            # futil-only builds do not compile updater_utils.c; remove the
            # CSME command whose symbols are supplied by that updater module.
            sed -i '/futility\/platform_csme\.c/d' futility/Makefile.inc
          '' + pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isDarwin ''
              substituteInPlace host/Makefile.inc \
                --replace-fail 'ar qcT' 'ar qc'
              substituteInPlace Makefile \
                --replace-fail 'LDFLAGS += -Wl,--gc-sections' \
                               'LDFLAGS += -Wl,-dead_strip'
          '' + ''
              cat > scripts/getversion.sh <<'EOF'
              #!/bin/sh
              echo 'const char futility_version[] = "c71f57be588c1ca69052e6f7208cc16437db513e";'
              EOF
              chmod +x scripts/getversion.sh
          '';

          makeFlags = [
            "ARCH=${if pkgs.stdenv.hostPlatform.isAarch64 then "arm64" else "x86_64"}"
            "SDK_BUILD=1"
            "USE_FLASHROM=0"
          ] ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isDarwin [ "HAVE_MACOS=1" ];
          buildFlags = [ "futil" ];
          preBuild = pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isDarwin ''
              export CPPFLAGS="-I${./nix/vboot-macos-compat} -include vboot_macos_compat.h"
          '';

          installPhase = ''
            runHook preInstall
            mkdir -p "$out/bin"
            cp build/futility/futility "$out/bin/futility"
            ln -s futility "$out/bin/vbutil_kernel"
            runHook postInstall
          '';

          meta = {
            description = "Pinned ChromeOS vboot kernel signing tools";
            license = pkgs.lib.licenses.bsd3;
            platforms = supportedSystems;
          };
        };
      mkCoachzLimine =
        pkgs:
        let
          upstream = pkgs.limine.override {
            targets = [ "aarch64" ];
            biosSupport = false;
            pxeSupport = false;
          };
        in
        upstream.overrideAttrs (old: {
          pname = "limine-coachz";
          version = "12.4.0-mpidr-affinity";
          src = pkgs.fetchurl {
            url = "https://github.com/Limine-Bootloader/Limine/releases/download/v12.4.0/limine-12.4.0.tar.xz";
            hash = "sha256-gVXcK/jCkKYF0OJ/A1sc5rL9XR81nF/AlfkATXzYmpM=";
          };
          patches = (old.patches or [ ]) ++ [ ./nix/limine-aarch64-mpidr-affinity.patch ];
          meta = (old.meta or { }) // {
            # The AArch64 EFI cross-build works with nixpkgs' LLVM stdenv on Darwin.
            badPlatforms = [ ];
          };
        });
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = import nixpkgs { inherit system; };
        in
        {
          vboot-tools = mkVbootTools pkgs;
          coachz-limine = mkCoachzLimine pkgs;
        }
      );

      devShells = forAllSystems (
        system:
        let
          pkgs = import nixpkgs { inherit system; };
          rustToolchain = scarlet-rust-toolchain.packages.${system}.scarlet-rust-toolchain;
          pythonEnv = pkgs.python3.withPackages (ps: [ ps.pyusb ]);
          vbootTools = mkVbootTools pkgs;
          coachzLimine = mkCoachzLimine pkgs;

          cargo-scarlet = scarlet-sdk.packages.${system}.cargo-scarlet;
          cargo-scarlet-plugin-limine = scarlet-sdk.packages.${system}.cargo-scarlet-plugin-limine;
          nativeRuntime = scarlet-sdk.packages.${system}.scarlet-native-runtime;

          # Current nixpkgs patches PyUSB to use libusb's absolute store path.
          # Only an unpatched/older PyUSB on Darwin should need this fallback.
          darwinLibusbFallback = pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isDarwin ''
            if ! python3 -c \
              'import usb.backend.libusb1 as backend; raise SystemExit(backend.get_backend() is None)' \
              >/dev/null 2>&1
            then
              export DYLD_LIBRARY_PATH="${pkgs.lib.makeLibraryPath [ pkgs.libusb1 ]}''${DYLD_LIBRARY_PATH:+:$DYLD_LIBRARY_PATH}"
              echo "PyUSB could not locate libusb directly; enabled the Darwin DYLD_LIBRARY_PATH fallback." >&2
            fi
          '';
        in
        {
          default = pkgs.mkShell {
            packages = [
              # Keep the composed interpreter early so host Python cannot shadow it.
              pythonEnv
              pkgs.libusb1
              pkgs.git
              pkgs.curl
              pkgs.gnumake
              pkgs.cmake
              pkgs.ninja
              pkgs.pkg-config
              pkgs.gnutar
              pkgs.which
              pkgs.ripgrep
              pkgs.llvmPackages.llvm
              pkgs.clang
              pkgs.lld
              pkgs.dtc
              pkgs.e2fsprogs
              pkgs.pkgsCross.aarch64-multiplatform.buildPackages.gcc
              cargo-scarlet
              cargo-scarlet-plugin-limine
              pkgs.mtools
              pkgs.minicom
              vbootTools
              coachzLimine
            ];

            hardeningDisable = [ "zerocallusedregs" ];

            shellHook = ''
              export PATH="${rustToolchain}/bin:$PWD/scripts:$PATH"
              export SCARLET_RUST_ACTIVE_BIN="${rustToolchain}/bin"
              export CC="${pkgs.stdenv.cc}/bin/cc"
              export CXX="${pkgs.stdenv.cc}/bin/c++"
              export HOST_CC="$CC"
              export HOST_CXX="$CXX"
              export VULKAN_HEADERS_SOURCE_DIR="${vulkan-headers-src}"
              export SCARLET_NATIVE_RUNTIME="${nativeRuntime}"
              export SCARLET_LLVM_LIBC_SOURCE_DIR="${scarlet-sdk.inputs.llvm-src}/libc"
              export SCARLET_NATIVE_CC="${pkgs.llvmPackages_21.clang-unwrapped}/bin/clang"
              export SCARLET_NATIVE_CXX="${pkgs.llvmPackages_21.clang-unwrapped}/bin/clang++"
              export SCARLET_NATIVE_AR="${pkgs.llvmPackages_21.llvm}/bin/llvm-ar"
              export SCARLET_NATIVE_RANLIB="${pkgs.llvmPackages_21.llvm}/bin/llvm-ranlib"
              export SCARLET_NATIVE_NM="${pkgs.llvmPackages_21.llvm}/bin/llvm-nm"
              export LIBCLANG_PATH="${pkgs.llvmPackages_21.libclang.lib}/lib"
              export CARGO_NET_GIT_FETCH_WITH_CLI=true
              # Native Scarlet C dependencies provide their own libc shim.
              # Linux GCC leaks glibc headers into those builds (for example
              # Moonlight's mbedTLS). Keep per-application compiler overrides
              # available by using cc-rs's fallback variables, as Scarlet does.
              export TARGET_CC="${pkgs.llvmPackages.clang-unwrapped}/bin/clang"
              export TARGET_AR="${pkgs.llvmPackages.llvm}/bin/llvm-ar"
              export TARGET_RANLIB="${pkgs.llvmPackages.llvm}/bin/llvm-ranlib"
              export SCARLET_PROBE_CC="${pkgs.llvmPackages.clang-unwrapped}/bin/clang"
              export SCARLET_COACHZ_LIMINE_EFI="${coachzLimine}/share/limine/BOOTAA64.EFI"
              ${darwinLibusbFallback}
            '';
          };
        }
      );
    };
}
