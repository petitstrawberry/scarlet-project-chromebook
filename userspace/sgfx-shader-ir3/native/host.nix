# Source-built host compiler for native tests; no Scarlet C runtime or cross tools.
{ pkgs, llvmSource }:
let
  python = pkgs.python3.withPackages (ps: [ ps.mako ps.pyyaml ps.setuptools ps.packaging ]);
in
pkgs.stdenv.mkDerivation {
  pname = "scarlet-ir3-compiler-host";
  version = "3f1b217bafff-abi1";
  src = import ./mesa-source.nix { inherit pkgs; };
  nativeBuildInputs = [ python pkgs.meson pkgs.ninja pkgs.pkg-config
    pkgs.bison pkgs.flex pkgs.llvmPackages_21.llvm ];
  buildInputs = [ pkgs.expat ] ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [ pkgs.libdrm ];
  dontUnpack = true;
  dontConfigure = true;
  # Match the exact compiler archive recorded in provenance.json.
  dontFixup = true;
  strictDeps = true;
  hardeningDisable = [ "all" ];
  buildPhase = ''
    runHook preBuild
    export PYTHONDONTWRITEBYTECODE=1
    python3 ${./.}/build.py --source "$src" --work "$PWD/native" --jobs "$NIX_BUILD_CORES"
    runHook postBuild
  '';
  doCheck = true;
  checkPhase = ''
    runHook preCheck
    # Exercise the private Scarlet formatting adapter on the host too.
    $CXX -std=c++17 -O2 -UNDEBUG -I${llvmSource}/libc \
      ${./.}/platform/printf.cpp ${./.}/platform/printf_test.cpp -o printf-test
    ./printf-test
    runHook postCheck
  '';
  installPhase = ''
    runHook preInstall
    mkdir -p "$out/lib" "$out/share/sgfx-shader-ir3"
    cp native/lib/libscarlet_ir3.a native/lib/provenance.json "$out/lib/"
    cp ${./compiler.h} ${./mesa-inputs.json} ${./NOTICE} "$out/share/sgfx-shader-ir3/"
    runHook postInstall
  '';
  meta = {
    description = "Pinned Mesa IR3 host compiler archive for real SGFX shader tests";
    license = pkgs.lib.licenses.gpl2Only;
    platforms = [ "aarch64-darwin" "aarch64-linux" "x86_64-linux" ];
  };
}
