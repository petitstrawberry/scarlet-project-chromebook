# Scarlet Adreno native driver

`libsgfx_scarlet_adreno.so` exports `sgfx_backend_get_api_v2` and implements
SGFX's mapped-target backend ABI. Install it with `scarlet-adreno.sgfx-driver`
in `/lib/sgfx`; the loader matches the kernel's `qcom-adreno` backend ID.
The optional low-level programmable-driver extension is not advertised.

All backend objects, resource tables and completion receipts belong to this
library. Commands cross the boundary as ABI v2 records and are decoded against
the driver's own table. Resource retirement observes queue completion before
reusing a logical slot. Imported handles transfer ownership to the driver.
The SGFX loader retains the library for process lifetime, including while
asynchronous GPU workers run.

The adapter follows the MIT-licensed SGFX VirGL plugin at `20bb5b8c`;
`LICENSE-MIT` preserves its attribution. The linked Adreno implementation is
GPL-2.0-only. The driver has its own checked-in Cargo lockfile.

Run `cargo test --locked --manifest-path userspace/sgfx-backend-scarlet-adreno-plugin/Cargo.toml`
from the repository root. Besides ABI validation tests, the host integration
test discovers the installed manifest, dynamically loads the real host cdylib,
and calls its exported table. GPU rendering still requires CoachZ hardware.

The public `bundles/sgfx-adreno/bundle.toml` uses a cargo-scarlet Cargo layer
with `lib = "sgfx_scarlet_adreno"`, followed by a copy layer for the manifest.
The SDK owns compilation, target selection, caching, installation and locking.
Include this bundle after Scarlet's `sgfx-native` bundle. No project-specific
build or install script is needed.
