<!-- SPDX-License-Identifier: GPL-2.0-only -->

# A618 SGFX IR3 shader pack

This directory contains host-compiled Adreno 618 graphics shaders produced by
Mesa IR3 at commit `3f1b217baffffa00cb8f53e158713a33e1bd4632`.

The upstream source is [Mesa](https://gitlab.freedesktop.org/mesa/mesa.git),
commit `3f1b217baffffa00cb8f53e158713a33e1bd4632`. The relevant Mesa IR3,
A6xx descriptor XML, and Turnip state-emission sources are MIT-licensed; see
[`NOTICE`](NOTICE). The local producer and helpers are GPL-2.0-only, matching
this repository. The generated IR3, disassembly, and JSON files are project
artifacts distributed GPL-2.0-only. No Mesa source file is bundled here.

`mesa-metadata.json` schema version 2 is emitted directly from Mesa `ir3_shader_variant`
structures and `ir3_link_shaders()`. `packed-state.json` contains A6xx register
payload dwords derived from that metadata using the pinned Mesa XML field
locations and the pinned Turnip state-emission algorithms. It embeds the exact
SHA256 of `mesa-metadata.json`, allowing typed Rust metadata to reject drift.
Compiler-emitted state dependencies include merged-register mode, early
preamble, thread size, helper/LOD flags, prefetch end-of-quad, output flags,
clip/cull state, and output view/alias data; the packer consumes these fields or
fails explicitly for unsupported VPC state. Each `.bin` is the
exact `info.sizedwords * 4` instruction stream and is 128 bytes. Each `.disasm`
is Mesa `disasm_a3xx_stat(..., gpu_id=618)` output.

## Uniform layout

Both stages receive the same five direct constant vec4s (80 bytes): matrix
columns in `c0` through `c3`, followed by color in `c4`. The matrix calculation
is `column0*x + column1*y + column2*z + column3*w`, matching SGFX's column-major
`Transform` and the WGPU backend. Mesa reports `constlen=12` for VS and `8` for
FS after its own reserved constant allocations; only `c0..c4` are application
data. `imm_count_dwords` is zero for every variant.

Texture operations are lowered by Mesa to A6xx sampler pre-dispatch state.
Therefore the texture shaders intentionally have no cat5 instruction in their
instruction stream. Exact `sampler_prefetch` records and packed
`SP_PS_INITIAL_TEX_LOAD_*` payloads are present in the JSON files.

## Reproduction

The following is a complete, fresh-checkout procedure. It installs the local
producer into the just-cloned Mesa tree, checks and applies the contextual
Meson patch, then builds the producer and an independent `ir3-disasm` decoder.
Enabling softpipe/OpenGL only makes Mesa build full NIR rather than its
header-only NIR stub. No DRM device is opened.

```sh
PACK="$PWD/shared/adreno-a6xx-shader-pack/artifacts/a618"
WORK="$(mktemp -d)"
MESA="$WORK/mesa"
BUILD="$WORK/build"
OUT="$WORK/out"
git clone https://gitlab.freedesktop.org/mesa/mesa.git "$MESA"
git -C "$MESA" checkout --detach 3f1b217baffffa00cb8f53e158713a33e1bd4632
install -D -m 0644 "$PACK/sgfx_compile.c" \
  "$MESA/src/freedreno/ir3/tests/sgfx_compile.c"
git -C "$MESA" apply --check "$PACK/meson-target.patch"
git -C "$MESA" apply "$PACK/meson-target.patch"

nix-shell -p meson ninja pkg-config bison flex \
  python314Packages.mako python314Packages.pyyaml \
  python314Packages.setuptools python314Packages.packaging \
  zlib expat libxml2 --run \
  "meson setup --wipe '$BUILD' '$MESA' \
   -Dgallium-drivers=softpipe -Dvulkan-drivers= -Dplatforms= \
   -Dllvm=disabled -Dglx=disabled -Degl=disabled -Dgbm=disabled \
   -Dgles1=disabled -Dgles2=disabled -Dopengl=true \
   -Dtools=freedreno -Dbuild-tests=true -Dbuildtype=release && \
   ninja -C '$BUILD' src/freedreno/ir3/sgfx_compile \
                    src/freedreno/isa/ir3-disasm"

"$BUILD/src/freedreno/ir3/sgfx_compile" "$OUT"
python3 "$PACK/pack_state.py" --pack "$OUT"
python3 "$PACK/verify_pack.py" --pack "$OUT" --mesa "$MESA" \
  --ir3-disasm "$BUILD/src/freedreno/isa/ir3-disasm" --expected-pack "$PACK"
python3 "$PACK/verify_packed_state.py" --pack "$OUT"
(cd "$PACK" && shasum -a 256 -c SHA256SUMS)
```

`sgfx_compile.c` is deliberately installed rather than copied implicitly by a
tool. `meson-target.patch` contains only the matching Meson target addition,
and `git apply --check` is part of the documented sequence. The verifier
accepts explicit `--pack`, `--mesa`, and `--ir3-disasm` paths; it never relies
on a pre-existing `/tmp` checkout.

## Hash manifest

`SHA256SUMS` is the complete inventory of stable, tracked files in this
directory other than itself: producer source, reproduction material, helper
scripts, provenance notice, generated binaries/disassemblies, and generated
JSON. It intentionally excludes all `*.log` files. Logs are ignored,
ephemeral execution evidence and belong under `.omo/evidence/`, not in a
release artifact manifest. Verify the shipped files with:

```sh
(cd shared/adreno-a6xx-shader-pack/artifacts/a618 && shasum -a 256 -c SHA256SUMS)
```

## Descriptor assumptions

Nearest/clamp sampler state is separate from shader code. Pinned Mesa
`tu_sampler.cc` selects nearest filters (`0`) and clamp-to-edge (`1`) on S/T/R;
from `a6xx_descriptors.xml` this makes sampler dword 0 equal `0x00000920`, with
dwords 1..15 zero for min/max LOD 0, no compare, and default border color.
Linear/clamp uses filter value `1` for XY min/mag and otherwise the same state.

For a linear, single-plane BGRA8 2D texture, use pinned `fdl6_view_init()` with
`FMT6_8_8_8_8_UNORM`, `TILE6_LINEAR`, one sample/level, identity view swizzle,
`TYPE=A6XX_TEX_2D`, and the real FDL layout values for pitch alignment, pitch,
layer size, width, height, depth, and base IOVA. The authoritative field map is
`src/freedreno/registers/adreno/a6xx_descriptors.xml`; words 4/5 encode
`base_iova` low/high plus depth. Do not substitute hardcoded layout guesses.
