# Real programmable A618 fixtures

These are actual Mesa IR3 compiler artifacts, not hand-written machine code or
fixed pipeline shader substitutions. They were compiled by `sgfx-shader-ir3`
from the accompanying WGSL with pinned Mesa
`3f1b217baffffa00cb8f53e158713a33e1bd4632`, metadata ABI 1, GPU 618.
The compiler disables preamble and fragment sampler prefetch for this emitter.

Reproduce with the `ir3-compile` example/CLI built with its Mesa feature:
`ir3-compile <vertex|fragment|compute> <entry> <input.wgsl> <output-prefix>`.
`procedural.wgsl` produces `vs` and `fs`; `compute.wgsl` produces `cs`;
`glass.wgsl` produces `blur_vertex`, `glass_vertex`, `horizontal`, `vertical`,
`glass`, and `erase`; `push.wgsl` and `storage.wgsl` produce compute entry `cs`
with prefixes `push` and `storage` respectively. Each prefix contains the full
128-byte-aligned allocation (`.bin`), metadata (`.json`), resource reflection
(`.bindings.json`), and normalized SPIR-V (`.spv`).

`../native_support/mod.rs` supplies reusable cases for both codegen tests and
`tests/a618-submit-validation`, which passes emitted relocations through the
production kernel validator. Host tests require no GPU or Mesa compiler linkage;
the validator's live compiler tests can replace the fixture artifacts with fresh
compilations of these same WGSL inputs.

The programmable feature is opt-in and does not advertise a device capability.
Current admission supports one through eight native color attachments, with
per-target blending and channel masks: BGRA8/RGBA8 UNORM and linear RGBA16Float
or RGBA32Float, plus optional Depth32Float. `BeginNativeRenderPass` carries each
attachment's load/store operations; `EndRenderPass` closes it. The original
`BeginRenderPass` and fixed pipeline path retain their single-BGRA contract.
Native float clears use A2D per-component binary16 (round toward zero) or
binary32 values. Packed uploads preserve source bits and destination row
padding. Equal-size, same-format linear float copies use bounded CP_MEMCPY
rows, preserving NaN payloads and signed zero without A2D conversion. Uploads
and copies flush prior image writes and invalidate consumers after CP waits.
Sampled float mip layouts use physical 8/16-byte texels. Parent
normalization must retain the actual float `storage_format`. Float/MRT hardware
execution is not established by host or kernel-validator tests.

Filtered same-format linear float blits support nearest/linear filtering,
scaling and X/Y flips using Mesa A2D IFMT3/4 and source half precision. D32
uploads stage packed four-byte texels in a zero-padded 64-byte-pitch buffer
then copy into typed TILE6_3 depth storage; D32 image copies require equal
extents and use nearest/no flips. These require the corresponding expanded
kernel validator; older frozen kernels reject them.

Sampled views support selected mip/layer ranges, D1/D1Array/D2/D2Array/Cube,
linear/sRGB interpretation, and single-level TILE6_3 D32 load/comparison.
Typed views retain original allocation identity and layer stride. Parent must
pass an explicit view for 1D/array/cube dimensions (untyped bindings default
to D2). RG8 sampling expects physical BGRA `[0,g,r,255]`; native R8 sampling
reads the existing alpha-byte expansion as logical `(r,0,0,1)`. Fixed alpha
mask behavior is unchanged. Upload conversion preserves encoded sRGB bytes.

Remaining bounds: up to 16 texture/sampler slots,
32 UBO/UAV slots, 256 vec4 constant registers. UBO offsets are 16-byte aligned;
byte sizes must be multiples of four and are padded without out-of-binding
reads. SSBO offsets are four-byte aligned: Mesa's 64-byte base plus
STARTOFFSETTEXELS preserves exact shader bounds and aliasing; relocation
authority conservatively includes the alignment prefix. Private memory,
preambles, sampler prefetch, multisample/viewport/layer interfaces,
tiled float layouts, depth mip/array allocations, filtered depth blits,
depth-only passes, and conflicting cross-stage
graphics UAV slot mappings fail explicitly. Compute has fixed local sizes with
at most 1024 invocations, 32 KiB shared memory, and direct group counts from 1
through 65535 per axis. Physical workgroup allocation uses
`max(metadata.shared_size, metadata.compute.req_local_mem)`; no new metadata
flag or ABI field is required. Barrier uniformity remains a compiler/frontend
validation limitation documented by the compiler owner.

Shared VS/FS storage bindings require Nash's canonical BindingMap and
compile_with_bindings; the full map and compiler cache version are part of
the parent cache key. The collision guard remains to reject legacy per-module
artifacts with conflicting slots.

`sampled_views.wgsl` was compiled with the local real Mesa IR3 CLI into
`depth_sample`, `array_sample`, and `cube_sample` artifacts. The depth shader
exercises both textureLoad and textureSampleCompareLevel; array/cube shaders
sample selected nonzero mip/layer views. Host tests verify emitted fields;
they do not substitute for hardware image checks.

`readonly_graphics.wgsl` supplies real `readonly_vs`/`readonly_fs` artifacts.
The current compiler emits UAV loads (`image_mapping.num_tex == 0`) for both.
The graphics offset regression tests those artifacts and an explicitly
overridden SSBO-to-texture metadata mapping. The override checks descriptor
address/authority consistency, not an ISAM ISA or hardware execution claim.

Native raster depth bias uses the public constant and slope factors unchanged,
with a zero clamp, as in Mesa's A6xx rasterizer. Each draw resets the previous
bias before applying its own state. The regression includes positive/negative
factors and a biased-to-unbiased transition in one D32 pass. Nonzero bias
requires the production validator's finite three-register burst and POLY_OFFSET
allowlist; it is not a hardware validation claim.

`draw_vs_core` / `draw_vs_khr` and `draw_fs` come from the shader compiler's
`draw_parameters_oracle` example (compiler cache version 7). Their vertex
`.spv` files are original SPIR-V 1.3 / SPV_KHR_shader_draw_parameters inputs,
not the WGSL generator template. Both compile to identical native code. The
real VS uses a kind-3 driver allocation at vec4 offset 4 and reads four driver
words; it does not consume Mesa's fifth `is_indexed_draw` word. The codegen
regression checks DrawIndex, signed BaseVertex, BaseInstance and subsequent
draw updates in one stream. `draw_parameters.oracle.json` retains the expected
RGBA values for the installed-driver test in `sgfx-adreno-check`.
Both UAV and texture aliases preserve the aligned base, STARTOFFSETTEXELS,
and expanded relocation size; the texture alias changes only access to READ.

Additional real fixtures: `mrt.wgsl` fragment entry `fs` produces the `mrt`
prefix and two independently calculated outputs (including HDR/negative values).
`workgroup.wgsl` entry `wide_shared` produces the copied compiler `wide_shared`
prefix; it has 4096 bytes of physical shared memory, zero argument hint, and
actual LDL/STL/BAR/FENCE instructions. Both are used by the production validator
harness as well as these host tests. General shader bodies, resource access,
linkage, and register allocation come from each compiled artifact.
