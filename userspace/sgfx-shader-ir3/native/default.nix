# Source-built Scarlet A618 compiler. No validation-directory dependencies.
{ pkgs, rustToolchain, nativeRuntime, llvmSource }:
let
  python = pkgs.python3.withPackages (ps: [ ps.mako ps.pyyaml ps.setuptools ps.packaging ]);
  llvm = pkgs.llvmPackages_21;
in
pkgs.stdenv.mkDerivation {
  pname = "scarlet-ir3-compiler";
  version = "3f1b217bafff-abi1";
  src = import ./mesa-source.nix { inherit pkgs; };
  nativeBuildInputs = [ python pkgs.meson pkgs.ninja pkgs.pkg-config
    pkgs.bison pkgs.flex llvm.clang-unwrapped llvm.llvm llvm.lld ];
  # The source hash check sees the pristine input, before any build overlay.
  dontUnpack = true;
  dontConfigure = true;
  dontFixup = true;
  strictDeps = true;
  hardeningDisable = [ "all" ];
  buildPhase = ''
    runHook preBuild
    export SCARLET_NATIVE_RUNTIME=${nativeRuntime}
    export SCARLET_RUST_TOOLCHAIN=${rustToolchain}
    export SCARLET_LLVM_LIBC_SOURCE_DIR=${llvmSource}/libc
    export SCARLET_NATIVE_CC=${llvm.clang-unwrapped}/bin/clang
    export SCARLET_NATIVE_CXX=${llvm.clang-unwrapped}/bin/clang++
    export SCARLET_NATIVE_AR=${llvm.llvm}/bin/llvm-ar
    export PYTHONDONTWRITEBYTECODE=1
    # Run the same formatter against host libc before cross compiling it.
    ${pkgs.stdenv.cc}/bin/c++ -std=c++17 -O2 -UNDEBUG -I${llvmSource}/libc \
      ${./.}/platform/printf.cpp ${./.}/platform/printf_test.cpp -o printf-test
    ./printf-test
    python3 ${./.}/scarlet-cross.py --output "$PWD/scarlet-cross.ini" --verify
    python3 ${./.}/build.py --source "$src" --work "$PWD/native" \
      --cross "$PWD/scarlet-cross.ini" --jobs "$NIX_BUILD_CORES"
    python3 ${./.}/check_link.py --archive "$PWD/native/lib/libscarlet_ir3.a" \
      --cross "$PWD/scarlet-cross.ini" --output "$PWD/compiler-root.elf"
    runHook postBuild
  '';
  installPhase = ''
    runHook preInstall
    mkdir -p "$out/lib" "$out/share/sgfx-shader-ir3"
    cp native/lib/libscarlet_ir3.a native/lib/provenance.json "$out/lib/"
    cp ${./compiler.h} ${./mesa-inputs.json} ${./NOTICE} "$out/share/sgfx-shader-ir3/"
    runHook postInstall
  '';
  meta = {
    description = "Source-built Mesa IR3 compiler for Scarlet A618 userspace";
    license = pkgs.lib.licenses.gpl2Only;
    platforms = [ "aarch64-darwin" "aarch64-linux" "x86_64-linux" ];
  };
}
