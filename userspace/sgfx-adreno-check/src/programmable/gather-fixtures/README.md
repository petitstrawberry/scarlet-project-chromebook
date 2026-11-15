# Gather hardware oracle sources

`gather.rs` embeds these portable WGSL and SPIR-V source modules and texture
inputs. It compiles through the installed SGFX DriverApi, draws to BGRA8 UNORM,
and checks every pixel against CPU-computed expectations. Both source formats
must also produce byte-identical readbacks. Native binaries are not embedded.

Eight cases cover RGBA gather components, a `(1,-1)` constant offset, ordinary
depth gather, and depth comparison with Less/LessEqual (including equality).
Each case draws a 4x4 target twice: **16 submissions, 256 checked pixels**.
Both vertex and fragment modules use the selected source format, entry `main`.
Texture/sampler bindings retain group 0, bindings 0/1 in the source SPIR-V.
Ordinary depth gather uses an ordinary sampler; only comparison cases use a
comparison sampler. `--compile-only` validates both source variants without
executing commands or reporting pixel PASS.

`oracle.json` describes the inputs, texel order, and both RGBA/BGRA expectations.
The `.expected.rgba8` files are logical RGBA references, not raw BGRA readbacks.
The executable independently computes expected values; it does not consume
those reference files. Component order follows the
[WGSL textureGather definition](https://www.w3.org/TR/WGSL/#texturegather).

Regenerate from the checkout root using a source-built **host** Mesa IR3 archive:

```sh
IR3_MESA_LIB_DIR=/path/to/host/lib cargo run --locked \
  --manifest-path userspace/sgfx-shader-ir3/Cargo.toml \
  --example gather_oracle -- \
  work/validation/ir3-runtime/artifacts/gather-hardware \
  userspace/sgfx-adreno-check/src/programmable/gather-fixtures
```

The generator uses the crate's pinned Naga fork, emits original-binding source
SPIR-V, then independently compiles WGSL/SPIR-V through Mesa and checks native
allocation and sampled-interface equality for all nine pairs (VS + eight FS).
Native dumps remain in the validation directory. Regeneration does not require
a GPU, and native equality is not evidence of correct GPU execution.

Parent integration is `mod gather;` and `gather::run(path)` in `programmable.rs`.
No added runtime dependencies are required. The compiler frontend changes use
cache version 7; Mesa native ABI 1 and the existing native archive are unchanged.
