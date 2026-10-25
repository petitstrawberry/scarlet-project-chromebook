# CoachZ file layers

These directories mirror the destination filesystem. The image builder copies
their contents to `/`; add a file at its intended relative path.

| Directory | Images | Use |
| --- | --- | --- |
| `common/` | initramfs and rootfs | Files needed in both images, including firmware license materials |
| `rootfs/` | rootfs only | Board settings and arbitrary persistent-image additions |
| `private/common/` | initramfs and rootfs | Local files excluded from Git |
| `private/rootfs/` | rootfs only | Local settings, credentials and arbitrary files excluded from Git |

For example, `rootfs/etc/example.conf` becomes `/etc/example.conf` in the
rootfs. `rootfs/opt/example/data.bin` becomes `/opt/example/data.bin`. Files in
`common/` are copied into both the boot initramfs and persistent rootfs; keep
large, runtime-only files in `rootfs/`.

Tracked layers are applied in the order `common/`, then `rootfs/` (rootfs
image only). Private layers follow both tracked layers, in the order
`private/common/`, then `private/rootfs/` (rootfs image only). Later files
replace earlier files at the same destination path.

## Local private files

The private directories exist in a fresh clone through their tracked
`.gitkeep` files. All other contents under `private/` are ignored by Git.

For example, `private/rootfs/etc/example.conf` becomes `/etc/example.conf`
and `private/rootfs/root/data.bin` becomes `/root/data.bin`. The repository's
`.gitignore` excludes their contents except the `.gitkeep` files; keep local
contents untracked. These files are included in locally generated images, so
Git exclusion does not make an image safe to publish.

Private directories use the same copy layers as the tracked directories.
Local input changes are picked up on image composition. Public CI builds
only the tracked contents. Removing a private file and rebuilding also
removes it from the newly generated image.

Firmware binaries live separately in `../firmware/qcom/` and are installed at
`/lib/firmware/qcom` in both images.
