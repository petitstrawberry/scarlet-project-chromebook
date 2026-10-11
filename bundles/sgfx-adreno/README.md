# Adreno SGFX bundle

Build and install the AArch64 Adreno ABI v2 shared driver using cargo-scarlet:

```toml
[[images.rootfs.layers]]
kind = "bundle"
source = { git = "https://github.com/petitstrawberry/scarlet-project-chromebook", rev = "4f4d0748ebbbd8f8ae30aaa18e2a58a3952e74ec" }
subdir = "bundles/sgfx-adreno"
```

Include after Scarlet's `sgfx-native` bundle. Requires cargo-scarlet's `lib`
Cargo-layer selector (SDK revision `ad692dc` or a successor containing it).
Then run `cargo scarlet image --project <project> --release`.
The SDK fetches the source, builds `libsgfx_scarlet_adreno.so`, installs it and
`scarlet-adreno.sgfx-driver` in `/lib/sgfx`, and records the output in
`scarlet.lock`. No local sibling checkout, prebuilt archive, or bundle script
is required. Relative Cargo dependencies stay within the fetched repository.

The driver provides the mapped-target ABI, including mip allocation/sampling,
array-layer uploads and texture blits. Programmable pipelines are not exposed.
Client selection follows the published SGFX facade's backend policy; the
bundle supplies the loadable driver and does not patch client sources.
