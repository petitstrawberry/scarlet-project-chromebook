# Adreno SGFX bundle

Build and install the AArch64 Adreno ABI v2 shared driver using cargo-scarlet:

```toml
[[images.rootfs.layers]]
kind = "bundle"
source = { git = "https://github.com/petitstrawberry/scarlet-project-chromebook", rev = "984eb48e4919bacce358b3bce4170375ab635201" }
subdir = "bundles/sgfx-adreno"
```

Include after Scarlet's `sgfx-native` bundle. Requires cargo-scarlet's `lib`
Cargo-layer selector (SDK revision `ad692dc` or a successor containing it).
Then run `cargo scarlet image --project <project> --release`.

When the bundle is consumed from the same checkout, reference the local
bundle file instead of a Git revision so the driver always matches the
checkout's `userspace/` sources. The path is relative to the project
directory:

```toml
[[images.rootfs.layers]]
kind = "bundle"
path = "../../bundles/sgfx-adreno/bundle.toml"
```

The SDK fetches the source, builds `libsgfx_scarlet_adreno.so`, installs it and
`scarlet-adreno.sgfx-driver` in `/lib/sgfx`, and records the output in
`scarlet.lock`. No local sibling checkout, prebuilt archive, or bundle script
is required. Relative Cargo dependencies stay within the fetched repository.

The driver provides the mapped-target ABI, including mip allocation/sampling,
array-layer uploads and texture blits. Programmable pipelines are not exposed.
Client selection follows the published SGFX facade's backend policy; the
bundle supplies the loadable driver and does not patch client sources.
