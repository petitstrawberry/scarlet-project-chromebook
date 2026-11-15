# NV12 logical reconstruction oracle fixtures

Decoder-backed pixel checks for load, gather, transparent-border sampling and
sixteen public sampler slots. Run `sgfx-adreno-check --programmable` with
`--nv12-reconstruction-only`, `--nv12-border-only` or `--nv12-slots-only`.

`frame.h264` is a deterministic baseline Annex-B frame, encoded by pinned FFmpeg
8.1.2/libx264. `reference.nv12` is its actual software decode, not the encoder's
input and not a generated expected RGB image. A second decode is byte-identical.
All 16,129 luma and all 3,969 UV 2x2 neighborhoods contain four distinct samples.
See `provenance.json` for commands, formulas, statistics, and hashes.

`shader.wgsl`, `vs.spv`, and `fs.spv` use the public sampled-texture interface.
WGSL uses group0 texture binding0 and sampler bindings1..4. SPIR-V uses texture
group2/binding0 and independent samplers group3/bindings0..3. The SPIR-V files
are generated from this source by the pinned vendored Naga and re-imported and
validated in the host proof. Sixteen actual gather operations cover components
0..3 and offsets (0,0), (1,-1), (-2,1), (-8,7). Five textureLoad operations cover
mips0/1/-1 and coordinate offsets; two explicit-level nearest samples supply
additional controls. No conversion is performed by the user shader.

The 128x24 primary output has rows:

| Rows | Operation |
| --- | --- |
| 0 | textureLoad(q,0) |
| 1,2 | textureLoad(q,1), textureLoad(q,-1), robust zero expected |
| 3,4 | nearest sampler A/B at (q+0.75)/128 |
| 5..8 | gather A, components0..3, no offset |
| 9..12 | gather B, components0..3, offset(1,-1) |
| 13..16 | gather A, components0..3, offset(-2,1) |
| 17..20 | gather B, components0..3, offset(-8,7) |
| 21,22 | textureLoad(q+(1,-1),0), textureLoad(q+(-1,1),0) |
| 23 | logical dimensions and level-count check |

The x probe axis is [-129,-2,-1,0,1,2,3,31,32,63,64,125,126,127,128,257].
The y axis is [-129,-1,0,1,63,126,127,128]. Their 128 products exercise negative,
interior, parity, edge, and multiple-period addressing. Coordinates are exact
quarter-texel values. For nearest controls, q+0.75 has the same nearest luma and
specified cosited chroma indices as a luma center but avoids sampler ties.
Gather's base is exactly q = floor(uv*128-0.5), including negative coordinates.

## Explicit reconstruction contract A

This implementation uses the following logical-texel policy because SGFX's public
Nv12/YCbCr record does not completely specify the subsampled edge distinction:

1. textureLoad guards mip and signed luma bounds **before** loading either
   plane. Invalid accesses return RGBA0; converting zero raw YCbCr is incorrect.
2. A valid luma texel q reconstructs chroma at floor((q+cosited)/2) independently
   per axis. Cosited odd ties advance positively. UV indices clamp to producer
   bounds; this is a sampler-independent logical texel color.
3. Gather computes four logical luma neighbors, applies the sampler address mode
   to each logical neighbor, reconstructs/converts each independently, then
   selects one component. Order xyzw is (0,1),(1,1),(1,0),(0,0). Offsets precede
   wrapping. ClampToBorder yields transparent RGBA0 for a logical border texel.
4. BT601/BT709, limited/full range, and both chroma locations per axis produce
   all16 conversion records. Conversion saturates RGB and yields alpha1 only
   for valid reconstructed color texels.

This decision is consistent with sampler-independent loads and four logical
color texels in a gather. It is **not** represented as a quote requiring these
Nv12 edge semantics in WGSL or Vulkan.

Pinned Mesa `3f1b217baffffa00cb8f53e158713a33e1bd4632`,
`src/vulkan/runtime/vk_nir_convert_ycbcr.c:152` supplies the cosited normalized
adjustment 1/(2*luma_dimension). Its raw Vulkan channel order is different from
A618's combined Y,Cb,Cr result; CPU conversion uses decoded Y and interleaved UV
bytes directly. `src/gallium/auxiliary/gallivm/lp_bld_sample_soa.c:797` documents
nearest floor and address mapping. SGFX `sgfx-core/src/ir/resource.rs` specifies
ClampToBorder as transparent black, including zero alpha.
WGSL's ordinary gather order and offset rules are at
https://www.w3.org/TR/WGSL/#texturegather .

## Independent nearest controls and unresolved edges

Two additional 128x1 mapped targets render public canonical fixed-path nearest
sampling for the same probe coordinates and two different address modes. The
programmable nearest rows compare with these real controls, never treating their
pixels as the load/gather expected values. A separate CPU model B independently
addresses raw Y and UV coordinates. CPU assertions on nearest controls apply
only where A and B are both defined and agree. At divergent edges the oracle
logs actual/A/B values, checksums, and match counts; partial plane border color
and alpha are deliberately undefined in B rather than guessed. These diagnostics
remain a sampling-edge consistency audit, not a claimed sampling pass.

Columns71..72/rows3..20 switch to a distinct ordinary BGRA texture, then column73
switches back to NV12 in the same pipeline/pass. The SPIR-V sampler group stays
bound across texture-only group replacement. Ordinary alpha is nonconstant,
which catches a stale NV12 conversion even in alpha-component gathers.

The future hardware run covers both languages, all16 conversions, four address
modes, two gather filter settings, two sweeps, and 512 submissions. Every mapped
target has PRESENT|RENDER_ATTACHMENT. The only NV12 producer is the public H264
video-client shared-image API. A duplicated producer handle is transferred
through public YCbCr import; **both** submissions wait for COMPLETE before
public release_import and before reimporting the same logical TextureId.
Decoder lifetime ends before rendering; the retained immutable handle leases
its backing. No fabricated handles or raw packets are used.

After promotion by parent:
`sgfx-adreno-check --programmable --nv12-reconstruction-only`
Optional `--nv12-reconstruction-dump=DIR` retains every actual readback with
language/sweep/conversion/mode/filter/target names. CPU checker tests use
explicitly synthetic CPU byte arrays only to test failure detection; these are
never uploaded, called device readback, or used to claim hardware completion.
