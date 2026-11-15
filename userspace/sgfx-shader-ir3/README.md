# A618 runtime compiler

This crate validates caller WGSL or SPIR-V through Naga 24 and compiles the chosen
vertex, fragment, or compute entry with source-built Mesa NIR + IR3 for A618.
There are no precompiled shader substitutions, subprocesses in `compile`, GPU
calls, or CPU rendering fallbacks. Successful compilation is **not** a claim of
GPU execution support or isolation. The caller must supply the complete PM4,
resource bindings, varying linkage, lifetime tracking, and GPU address isolation.

```rust,ignore
use sgfx_shader_ir3::{compile, validate, Source, Stage};
validate(Source::Wgsl(source))?; // checks the entire module, without selecting an entry
let shader = compile(Source::Wgsl(source), Stage::Fragment, "glass")?;
```

Features:

- Default `mesa`: native compiler + validated frontend + `std`.
- `frontend` without defaults: Naga validation, no native link. `compile` returns
  an explicit missing-compiler error; it never manufactures a shader.
- No default features: `alloc`/`no_std` artifact/metadata types only, for codegen.

The current language profile supports buffer uniforms, read-only/writable SSBOs,
sampled/depth textures, write-only 2D RGBA8 storage textures, separate samplers,
128 bytes of push constants, fixed
compute local sizes up to 1024 invocations, and up to 32 KiB of workgroup storage.
Shared arrays/vectors, dynamic indexing, atomic RMW/compare-exchange,
`workgroupBarrier` and `workgroupUniformLoad` lower to actual IR3 operations.
Naga emits WGSL zero-initialization stores and a rendezvous barrier. Only globals
used by each entry consume its shared budget; non-compute shared access is rejected.
The frontend checks Naga's logical storage layout (conservative relative to
IR3's scalar-aligned layout); native compilation also checks A618's physical limit.
Binding arrays, storage-image formats/access beyond write-only 2D RGBA8, 64-bit scalar types, and capabilities outside the
validator's profile return errors. This does not advertise the SGFX full capability set.

### Remaining language/profile boundaries

The frontend enables Naga `PUSH_CONSTANT`, `PRIMITIVE_INDEX` and the sample
interface syntax gate `MULTISAMPLED_SHADING`. Fragment
`primitive_index` uses Mesa's `nir_lower_sysvals_to_varyings` pass and actual
A618 primitive assembly: the flat `VARYING_SLOT_PRIMITIVE_ID` input's `inloc`
is selected by VPC `PRIMIDLOC`, with the VFD/PC primitive-ID bits enabled. No
vertex-generated or constant ID is substituted. Codegen resets both enable bits
and the unused location when returning to a plain fragment shader. Compiler
cache version 9 introduced this primitive-ID lowering. The
`--programmable --primitive-index-only` diagnostic checks distinct IDs for two
triangles, interpolated UVs, plain-shader transition and replay in WGSL/SPIR-V.
Host validation does not certify hardware execution.

Compiler cache version 10 admits fragment `sample_index` and perspective/linear
sample interpolation only under the explicit single-sample profile. The new
`scarlet_ir3_compile_with_samples` C entry requires `rasterization_samples=1`
for fragments and 0 for vertex/compute; other counts fail. Rust requires this
entry, so an old native archive cannot silently compile the new syntax. The
original C entry remains a wrapper for existing callers. Metadata records this
profile; legacy fixtures omit it. Result/C metadata ABI remains version 1.

Mesa's official `nir_lower_single_sampled`, run after I/O lowering, replaces
SampleId with zero and sample/centroid barycentrics with pixel-center
barycentrics while preserving perspective versus linear interpolation. Only
then is SPIR-V's sample-frequency flag cleared. Sample-mask input retains its
hardware coverage/helper path (`lower_sample_mask_in=false`). Codegen rejects
wrong explicit sample counts and residual sample/centroid inputs. The public
texture descriptors have no sample count, and actual multisampled color/depth
resources are rejected in WGSL and SPIR-V. Multiview, descriptor arrays and
geometry/tessellation stages remain outside this profile.

The `--programmable --single-sample-only` diagnostic checks sample index zero,
nonconstant perspective and linear values with clip W=1,2,4, fine x/y derivatives,
and sample/center equivalence against analytic pixel-center values, in WGSL and
SPIR-V over two replays. This requires rebuilt host/Scarlet native archives and
plugin; it does not require new kernel register admission. Hardware validation
is separate from compiler and strict-validator checks.

Sample-mask input is admitted. Sample-mask output now has native export/coverage
state, including invalid-register/bit reset when switching to a plain fragment
shader, with no new compiler ABI or archive requirement. Its dedicated
`sgfx-adreno-check` oracle checks single-sample color/depth coverage and transition
state using WGSL and original SPIR-V; hardware validation is separate from host
PM4/validator tests.

Stencil export, multiple viewports/layers, descriptor arrays, 64-bit scalar
extensions, and storage-image shapes/access beyond the public write-only 2D RGBA8
contract remain unsupported. Descriptor maps allow 16 assignments per namespace,
shared storage is capped at 32 KiB and workgroups at 1024 invocations. These device
limits are distinct from the public constructor's structural size limits.

## Shared pipeline binding map

For separately compiled VS/FS modules, use `compile_with_bindings` with the same
canonical `BindingMap` for both stages. The backend constructs the map from the
pipeline layout; the compiler does not silently assign incompatible per-module
UAV slots.

```rust,ignore
use sgfx_shader_ir3::{BindingAssignment, BindingMap, ResourceKind, compile_with_bindings};
let map = BindingMap::new(vec![
    BindingAssignment { group: 0, binding: 0, kind: ResourceKind::StorageBuffer, slot: 0 },
    BindingAssignment { group: 1, binding: 0, kind: ResourceKind::StorageBuffer, slot: 1 },
])?;
let vs = compile_with_bindings(Source::Wgsl(vertex_source), Stage::Vertex, "vs", &map)?;
let fs = compile_with_bindings(Source::Wgsl(fragment_source), Stage::Fragment, "fs", &map)?;
```

Assignments are sorted by original `(group,binding)` and exposed read-only via
`assignments()`. `BindingMap` is `Clone + Eq + Hash` and available without std or
native linkage. Duplicate original identities, conflicting `(kind,slot)` pairs,
and slots outside `0..16` are rejected. The four descriptor namespaces are
independent: a UBO, SSBO, texture, and sampler can all use native slot 0. The
maximum map holds 64 assignments (16 per kind), not 16 across all kinds.

Only resources used by the selected entry must appear; missing or wrong-kind
assignments are errors. Extra entries for other stages are allowed. Reflection
keeps the original `(group,binding)`, selected entry's access/layout information,
and chosen slot. Unused declarations and other entries do not leak into reflection,
even if they declare the same original binding with a different buffer layout.
Internal SPIR-V descriptor sets separate resource namespaces; the exported SPIR-V
is already normalized and must not be treated as the original source interface
when reusing an original-identity map.

The cache key must include the **entire canonical map**, original source/module
generation, stage, entry name, `COMPILER_CACHE_VERSION` (currently 10), and
`MESA_REVISION`. Do not reuse an entry-only cache or key only the stage's reflected
subset. A map differing only in another stage's assignment is still a distinct
pipeline map. Module lifetime/generation validation must precede a cache hit.
For a process-local cache whose compiler binary and module contents are fixed,
`(entry including module generation, BindingMap)` is sufficient: compiler/Mesa
identity is implicit in that process. Persistent or cross-version caches must
encode compiler/Mesa identity and source content explicitly.

Legacy `compile` retains its dense per-module, 16-total-binding behavior; use the
shared map API for pipelines with separate shader modules or larger combined
interfaces. The map API changes only the Rust frontend, requires no native archive
rebuild, and preserves artifact/C ABI 1. Storage-view byte offsets remain descriptor
state: this compiler does not add hidden rebasing constants or alter SSBO addresses.

## Sampled resource interface

`CompiledShader::sampled_bindings()` (also available on `Metadata`) returns
`Result<Option<Vec<SampledBinding>>, Error>`. This API and its types are available
with no default features. `None` identifies old artifacts without reflection;
`Some([])` means a freshly compiled entry with no sampled resources. Malformed
or unknown-version metadata is an error. Fresh runtime pipeline admission should
require `Some`, without invalidating older offline codegen fixtures.

Each `SampledBinding` records original `group`, `binding`, mapped `slot`, and `ty`:

- `SampledBindingType::Texture { dimension, arrayed, sample_type, multisampled }`
  records D1/D2/D3/Cube, arrayness, Float/Sint/Uint/Depth, and sample count class.
- `SampledBindingType::Sampler { comparison }` distinguishes ordinary and
  comparison samplers; it does not record the bound sampler's compare function.

Only the selected entry's used globals appear, even when other entries reuse the
same original binding with a different type. The backend must compare these
requirements against its pipeline layout and bound texture views/samplers. The
coarse `Binding.kind` or a canonical binding map alone cannot do this validation.
Reflection represents source types, not a promise that every type is executable
by a particular SGFX profile. In particular D3/multisampled/cube-array views must
still be rejected if outside that profile.

Metadata JSON stores this as `sgfx_sampled_interface: { version: 1, bindings: [...] }`
in the existing flattened `Metadata.extra`; no fields were added to `Binding` or
`CompiledShader`. C ABI 1 is unchanged. Compiler cache version 8 adds public
write-only 2D RGBA8 storage textures and requires a rebuilt native archive:
image dereferences lower to real IR3 image operations, with a fixed 16-slot SSBO
prefix so independently compiled graphics stages use the same image IBO base.
The typed variant is `SampledBindingType::StorageTextureRgba8WriteOnly`;
`Binding.kind` remains `Texture`, with `writable=true`. Its pipeline map shares
the texture namespace, while its physical IBO slot is `16 + Binding.slot`.
Sampled textures/samplers still use their independent texture/sampler tables.
Native admission must check this typed interface, image STORAGE usage, format,
view range and write authority; changing a sampled descriptor into writable
memory without those checks is invalid.

WGSL input is bounded at 4 MiB and SPIR-V at 1,048,576 words, matching the public
SGFX source bounds. Executable allocations remain bounded at 16 MiB and metadata
at 1 MiB. The backend CPU artifact cache's smaller source cutoff only bypasses
caching; it does not reject a shader.

`cargo run --example storage_oracle -- OUTPUT_DIRECTORY` regenerates the original-
binding WGSL/SPIR-V pairs for `sgfx-adreno-check`'s storage/colorless oracle and
checks identical native executable output. Host compilation is not GPU execution.

Depth load, ordinary depth sampling, implicit/explicit-level comparison, depth
2D arrays/cubes, WGSL comparison gather, and explicit-level comparison in compute
have native compiler coverage. Tests inspect real IR3 shadow operations with
independent texture/sampler descriptor indices and roundtrip ordinary depth
operations through source SPIR-V. These are compiler tests, not GPU execution tests.
The compiler-owned Naga 24 importer extension reads `OpImageGather` and
`OpImageDrefGather`, including
portable constant offsets, depth 2D arrays/cubes, and comparison sampler parameters
passed through helper functions. Unsupported image operand flags and malformed
counts are errors. Original WGSL and imported SPIR-V produce identical native code
for these regression cases. Ordinary gather preserves components 0..3 across
float/signed/unsigned textures and rejects nonconstant/out-of-range components.
Storage-image support remains unimplemented; current SGFX public API includes
write-only 2D RGBA8 `StorageTexture`, so this is an in-scope remaining gap.

Generate reproducible source pairs and GPU pixel oracles without accessing a GPU:

```sh
cargo run --locked --manifest-path userspace/sgfx-shader-ir3/Cargo.toml \
  --example gather_oracle -- work/validation/ir3-runtime/artifacts/gather-oracle
```

`oracle.json` specifies bindings, texture inputs, pipeline/sampler state, and
expected logical RGBA8 pixels for six cases. `*.source.spv` retains original
binding identities for runtime compilation. Run each WGSL and SPIR-V path and
compare actual readback to the CPU oracle (tolerance one byte per channel), not
only to each other. Gather ordering follows the linked WGSL specification. This
generator checks native binary equality but does not claim GPU execution.

Naga remains version 24.0.0, supplied by the private `vendor/naga` path dependency.
`vendor/naga-upstream.json` records published-source hashes and the crates.io
checksum; `vendor/naga-ir3.patch` is the complete importer/writer delta. Licenses
are preserved. Consumers must refresh their Cargo lockfiles to resolve this local
Naga package and include the source directory in deployment/build checkpoints.
There is no change to the Mesa native archive or C ABI.

## Shader draw parameters

Source SPIR-V supports `DrawParameters` (core 1.3) and
`SPV_KHR_shader_draw_parameters` (older versions). BaseVertex, BaseInstance and
DrawIndex survive import and normalization; the writer emits the required
capability and extension. This does not add nonstandard WGSL builtin names.

```sh
cargo run --locked --manifest-path userspace/sgfx-shader-ir3/Cargo.toml \
  --example draw_parameters_oracle -- work/validation/ir3-runtime/artifacts/draw-parameters
```

The generator starts with a WGSL **template**, assigns the three builtins in Naga
IR and emits original SPIR-V, then compiles that actual source through the public
compiler API. It supplies core/KHR VS `.spv/.bin/.json/.bindings.json`, matching
flat integer FS input, and expected colors/driver words for nonindexed, indexed
negative BaseVertex, and subsequent-draw updates. The template itself is not an
equivalent WGSL shader. These tests inspect actual driver constant allocation,
not fabricated metadata. Native Mesa/C ABI are unchanged; GPU execution is separate.

## Artifact ABI 1

`CompiledShader` and all metadata types implement `Clone`, `Debug`, `PartialEq`.
See `src/metadata.rs` for the typed schema and `native/compiler.h` for ownership.

- Upload **all** `allocation` bytes at a 128-byte-aligned GPU address.
  `metadata.sizedwords` describes instructions only. `constant_data_offset` and
  `constant_data_bytes` locate embedded read-only data in the same allocation.
  The procedural test's VS has zero immediate dwords but nonempty constant data.
- `constlen` and `const_allocations` use vec4 units. `immediate_offset_dwords`
  and `imm_values` identify the separate immediate constant-register upload.
- `ubo_ranges` use byte units, both destination `offset` and source `[start,end)`.
  The `block` is the native UBO descriptor slot. Copy the source range into the
  constant file at `offset/4` dwords, clipped to `16*constlen` bytes.
  Range ends can be rounded to 64 bytes: **zero-pad outside the bound source**.
  Do not read beyond an application's buffer because a compiler range is padded.
- `driver_ubos` order is constant-data, driver-parameters, primitive-map,
  primitive-parameters. An index of -1 means absent. For constant-data, bind the
  descriptor at `allocation_gpu_address + constant_data_offset`; for UBO ranges
  selecting that block, source the embedded constant bytes, not an app binding.
- A6xx ordinary UBO descriptors are two dwords (GPU address + vec4 size), emitted
  by `CP_LOAD_STATE6` with `ST6_UBO`, stage shader state block. Emit descriptors
  for declared app slots and active driver slots, plus the constant uploads.
  Dynamic accesses not promoted to constants still use these descriptors.
  Mesa's `num_ubos` can be zero after lowering; use `num_app_ubos` and active
  `driver_ubos` indices to bound descriptor uploads.
- Legacy `compile` reserves dense slots in sorted original `(group,binding)` order
  across the module. `compile_with_bindings` uses the shared pipeline map instead. Reflection lists only bindings actually used by the selected entry,
  while slots remain stable across stages. Resource namespaces remain separate:
  glass uses UBO 0, texture 1, sampler 2. Texture/sampler instruction indices are
  independent. `num_samp` conservatively covers both descriptor arrays.
- `image_mapping` identifies SSBO reads implemented through texture descriptors;
  writes use UAV/IBO slots. `0xff` means unused, `0x80|slot` denotes an SSBO.
- Input/output slots and register IDs use pinned Mesa enums (VS generic attribute
  base, fragment varying slots, system value slots). Link VS outputs to FS inputs
  using slot, compmask, inloc, interpolation, and register IDs, not vector order.
- ABI 1 disables shader preamble optimization and pre-dispatch texture prefetch.
  This is a local compiler/device-info setting, not a global Mesa table mutation.
  Constants are uploaded explicitly. Ordinary IR3 optimization remains enabled.
- Compute exports fixed `local_size`, register/sysval IDs, shared/private memory
  requirements, branch stack, thread size, and descriptor mappings. Reject any
  returned state the submission implementation cannot actually configure.
  `shared_size` is physical shared allocation **bytes per workgroup**, not a
  register encoding. `compute.req_local_mem` is an independent kernel-argument
  hint and is zero for these WGSL programs. `has_barrier` requires real workgroup
  rendezvous/dispatch configuration; it also covers the initialization barrier.

For authoritative PM4 upload conventions, consult the pinned Mesa source:
`src/gallium/drivers/freedreno/a6xx/fd6_const.cc`,
`src/gallium/drivers/freedreno/ir3/ir3_const.h`, and
`src/freedreno/ir3/ir3_shader.h` (driver parameter structures).

Naga keeps SGFX Y-up clip coordinates (no optional Y flip). C ABI calls are
serialized by a Rust mutex because Mesa has process-global type/debug state.
Native buffers stay in the C allocator domain until copied into Rust and freed
by `scarlet_ir3_result_free`; never free them with the Rust allocator.

Naga 24 does not enforce workgroup barrier uniformity. This crate adds a
conservative dependency-graph analysis **after** Naga semantic validation for every
compute entry, for both WGSL and imported SPIR-V. `validate` and compilation reject
non-uniform barrier control flow and non-uniform `workgroupUniformLoad` pointers.
The analysis follows mutable local assignments, SSA load snapshots, helper arguments
and pointer effects, early returns, switch/loop exits, and cyclic loop dependencies.
A barrier before a divergent break is checked against subsequent iterations too.
Divergent branches that fully rejoin do not poison later unconditional barriers.

Uniform builtin values and uniform/push buffers seed uniform data; ordinary
storage/private/workgroup reads, atomics and texture values are conservatively
varying. A valid `workgroupUniformLoad` produces a uniform result. Uniform loop
induction variables, including the 128-invocation cross-wave reduction, stay valid.
Aggregate memory uses whole-cell dependencies; algebraically equivalent varying
expressions are not simplified. Some valid programs can therefore be conservatively
rejected. This is not a termination proof or a general shader execution-safety proof.
Analysis has explicit graph/statement/call budgets and returns an error on exhaustion.
No barrier is removed or weakened, and the checker does not alter generated code.

## Reproducible source build

The project flake exports `packages.<system>.scarlet-ir3-compiler` and sets
`IR3_MESA_LIB_DIR` automatically in `nix develop`. This is the production image
build path; it fetches pinned source and has no dependency on `work/validation`.

```sh
nix build --accept-flake-config .#scarlet-ir3-compiler
nix develop --accept-flake-config
```

Keep the project's normal userspace build configuration. The integrated native
plugin builds with Thin LTO and has passed graphics, compute, and glass GPU tests
on CoachZ. A separate standalone cdylib probe, outside that configuration, failed
with missing `.llvmbc` even without Mesa/Naga; disabling release LTO was a workaround
for **that probe only**, not a general compiler/plugin requirement. Its audited
DSO links with `--no-undefined`, no `DT_NEEDED`, no ELF TLS, and only optional weak
`environ` unresolved. Native C/C++ code is built with `-O3`.

### Host tests and Ubuntu CI

The separate `scarlet-ir3-compiler-host` package builds the same pinned Mesa source
for the host CPU/OS. The `ir3-host` shell selects that archive, the pinned Scarlet
Rust toolchain, and a matching host C/C++ linker/runtime. It avoids accidentally
linking a Nix-built archive against Ubuntu's unrelated system C++/glibc version,
and needs neither a Scarlet runtime nor GPU hardware.

From the repository root with Nix flakes enabled:

```sh
nix build --accept-flake-config .#scarlet-ir3-compiler-host --no-link --print-out-paths
export CARGO_TARGET_DIR="$PWD/target/ir3-native-host"
nix develop --accept-flake-config .#ir3-host --command cargo test --locked \
  --manifest-path userspace/sgfx-shader-ir3/Cargo.toml
nix develop --accept-flake-config .#ir3-host --command cargo test --locked \
  --manifest-path userspace/sgfx-codegen-adreno-a6xx/Cargo.toml --features programmable
nix develop --accept-flake-config .#ir3-host --command cargo test --locked \
  --manifest-path userspace/sgfx-backend-scarlet-adreno/Cargo.toml \
  --no-default-features --features std,programmable
```

`.github/workflows/sgfx.yml` runs this path in the `native-shader-compiler` job on
Ubuntu 24.04, plus formatting, metadata-only, and frontend-without-native checks.
The package also runs the real LLVM printf adapter's host differential tests.
Compiler tests execute actual WGSL/SPIR-V → Naga → Mesa IR3 compilation, including
the real glass shader and workgroup/atomic/barrier regressions. They verify
compiler artifacts and instructions; they do not claim GPU execution coverage.


Mesa is pinned to `3f1b217baffffa00cb8f53e158713a33e1bd4632`.
`native/mesa-inputs.json` checks all 12,829 source files by path and content hash.
Use a clean `git archive` extraction; do not pass a different checkout HEAD.
The build creates a private symlink overlay and never edits the supplied source.

From the project root (paths may be changed):

```sh
mkdir -p work/validation/ir3-runtime/mesa
git -C /path/to/mesa-repository archive 3f1b217baffffa00cb8f53e158713a33e1bd4632 \
  | tar -x -C work/validation/ir3-runtime/mesa
nix-shell -p meson ninja pkg-config bison flex python3Packages.mako \
  python3Packages.pyyaml python3Packages.setuptools python3Packages.packaging \
  expat llvmPackages.llvm --run \
  'python3 userspace/sgfx-shader-ir3/native/build.py \
    --source work/validation/ir3-runtime/mesa --work work/validation/ir3-runtime/host'
export IR3_MESA_LIB_DIR="$PWD/work/validation/ir3-runtime/host/lib"
export CARGO_TARGET_DIR="$PWD/work/validation/ir3-runtime/target"
# On macOS use Clang rather than a GCC cc that cannot find libc++:
export CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER=clang
cargo test --locked --manifest-path userspace/sgfx-shader-ir3/Cargo.toml
```

For Scarlet, enter the project's `nix develop` environment with its real native
SDK. Ensure Python has the dependencies listed above, and Meson/Ninja/llvm-ar are
on PATH. The cross file contains actual target compile/link probes; no Linux
identity or host headers are substituted.

```sh
export SCARLET_RUST_TOOLCHAIN="${SCARLET_RUST_ACTIVE_BIN%/bin}"
python3 userspace/sgfx-shader-ir3/native/scarlet-cross.py \
  --output work/validation/ir3-runtime/scarlet-cross.ini --verify
python3 userspace/sgfx-shader-ir3/native/build.py \
  --source work/validation/ir3-runtime/mesa --work work/validation/ir3-runtime/scarlet \
  --cross work/validation/ir3-runtime/scarlet-cross.ini
python3 userspace/sgfx-shader-ir3/native/check_link.py \
  --archive work/validation/ir3-runtime/scarlet/lib/libscarlet_ir3.a \
  --cross work/validation/ir3-runtime/scarlet-cross.ini \
  --output work/validation/ir3-runtime/compiler-root.elf
export IR3_MESA_LIB_DIR="$PWD/work/validation/ir3-runtime/scarlet/lib"
cargo build --locked --release --target aarch64-unknown-scarlet \
  --manifest-path userspace/sgfx-shader-ir3/Cargo.toml --bin ir3-compile
```

`build.rs` links the real static C++/C++ ABI/Scarlet runtime libraries from
`SCARLET_NATIVE_RUNTIME`. The compiler's emulated TLS is LLVM compiler-rt's real
implementation, using pthread keys/destructors; the archive contains no ELF TLS.
The strict audit checks every archive object and links both C ABI roots with no
unresolved strong symbols. Diagnostic-only memory streams use real temporary
files on Scarlet; normal compilation/metadata export uses in-memory buffers.
The port supplies real `stpcpy`, `getline` and AArch64 FP environment
save/restore operations. Mesa's printf family is privately namespaced and uses
source-built LLVM libc `printf_core` from the SDK-pinned LLVM source, including
floating-point, integer length/precision and positional arguments. The SDK's
integer-only Rust `vsnprintf` rejects formats Mesa needs; it is not used for
compiler formatting. Buffer, growing allocation and FILE sinks preserve length,
truncation and `%n` semantics. Every Scarlet compile checks the native formatter
ABI before entering Mesa and returns an error on failure; Mesa asserts remain.
`native/platform/printf_test.cpp` compares the adapter against host libc for
normal formats and tests single-pass allocation and FILE flushing. Optional disk-cache
setup and OS logging destinations are disabled; stderr diagnostics remain.

The standalone tool writes `.bin`, `.json`, `.bindings.json` and normalized
`.spv` files. For example:

```sh
ir3-compile fragment horizontal glass.wgsl /tmp/horizontal
```

Do not delete another task's source/build tree. Once integration no longer needs
this task's Mesa overlay, preserve the self-contained archives, provenance,
compiler executable, and validation artifacts, then remove temporary trees.

## Bound NV12 conversion and reconstruction (compiler cache12)

`compile_with_bindings_ycbcr` specializes readonly 2D NV12 texture slots with
matrix, range and chroma locations. `compile_with_bindings_ycbcr_samplers` also
accepts the exact sampler slots, address modes and min/mag filters. These inputs
participate in the backend's variant key and are checked against bound resources.
The additive `scarlet_ir3_compile_with_nv12_samplers` entry preserves result ABI1;
matching host and Scarlet compiler archives must be rebuilt together.

Filtering samples and numeric queries retain the established combined-format
path. `textureLoad` guards the signed mip and coordinates before fetching;
invalid loads return transparent zero. A valid logical luma texel reconstructs
chroma at `min(floor((q + cosited) / 2), plane_size - 1)`, then converts and
saturates RGB. `textureGather` addresses each logical neighbor, reconstructs
and converts its color independently, then selects the requested component.
Gather order is (0,1), (1,1), (1,0), (0,0); offsets precede addressing.

Transparent-border samples reconstruct logical colors with nearest or bilinear
filtering, including alpha coverage at edges. Both programmable and generated
fixed NV12 border draws use this lowering. Ordinary fixed NV12 draws keep their
canonical shader path. An internal nearest/clamp sampler avoids sampler ties
and independent plane wrapping without creating additional image handles.
The sixteen public sampler slots remain available; the internal seventeenth
sampler does not add a seventeenth texture or relax image/resource validation.

Version2 `sgfx_nv12_profile` metadata records conversion and sampler identities,
instruction counts and the internal sampler slot. Cache identity and descriptor
emission validate this metadata. NV12 remains imported, readonly/SAMPLED,
non-array, one mip and one layer. Non-NV12 lowering, batching, submission,
retirement and GPU frequency control are unchanged.
