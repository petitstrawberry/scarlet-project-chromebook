# Private Naga 24.0.0 source

This is a production path dependency, not a temporary dependency cache. Source,
build script, package manifests and licenses come from the published naga 24.0.0
crate (SHA256 e380993072e52eef724eddfcde0ed013b0c023c3f0417336ed041aa9f076994e).
Original per-file hashes are recorded in `naga-upstream.json`.

Only `src/front/spv/image.rs`, `src/front/spv/mod.rs`,
`src/back/spv/writer.rs`, and `src/front/wgsl/{lower/mod.rs,error.rs,tests.rs}`
differ from that source;
`naga-ir3.patch` contains their full delta (zero-context unified diff; apply with
`git apply --unidiff-zero` against the hash-verified pristine source). The patch imports OpImageGather and OpImageDrefGather
using Naga's native ImageSample/gather IR, accepts portable constant offsets,
rejects unsupported operands/counts, and propagates comparison sampler types to
helper parameters as well as globals. It also admits DrawParameters capability/KHR extension and emits those required
declarations when writing BaseVertex/BaseInstance/DrawIndex. It preserves other
WGSL and SPIR-V paths. The narrow WGSL delta implements
`textureSampleBaseClampToEdge` for non-array, non-MSAA2D f32 sampled textures
with ordinary samplers: query base dimensions, clamp to half-texel centers and
sample explicit level0. It adds no external-texture or resource capabilities.
No dependency version or Mesa ABI is changed.

The separate workgroup uniformity checker belongs to the compiler's
`src/uniformity.rs`, not to this upstream fork. Avoid wholesale formatting or
unrelated upstream edits. The existing upstream warnings under the pinned Scarlet
Rust toolchain are retained; they were suppressed by Cargo for registry sources.

The registry archive omits standalone license files. `naga-licenses.json` records
the license URLs/hashes fetched from the exact published crate repository commit.
