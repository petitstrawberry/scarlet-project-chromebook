# CoachZ Adreno firmware

This directory vendors the two binary firmware files required by the Adreno
A618 driver. They are imported byte-for-byte from the official linux-firmware
repository at the pinned revision below; image creation performs no download.

Upstream repository: <https://gitlab.com/kernel-firmware/linux-firmware>

Pinned revision: `8c7fac62c0d1c3b8915f596effc1ef6e95fd6b5f`

| Upstream file | SHA-256 | System image path | Scarlet ABI path |
| --- | --- | --- | --- |
| `qcom/a630_gmu.bin` | `da8d9b1b1f5c1a0b311f32567093b4828f3c80031dd8435f91ac13c664e173a6` | `/system/scarlet/lib/firmware/qcom/a630_gmu.bin` | `/lib/firmware/qcom/a630_gmu.bin` |
| `qcom/a630_sqe.fw` | `1c21b527d9183487cc550dabbb3f43e555df5a977a461934fc61f0635a9aa90c` | `/system/scarlet/lib/firmware/qcom/a630_sqe.fw` | `/lib/firmware/qcom/a630_sqe.fw` |

These files are not distributed under the repository's GPL license. The
linux-firmware `WHENCE` entry identifies them as redistributable under QTI's
separate terms. The complete upstream `LICENSE.qcom` and `NOTICE.qcom` files
are preserved as `LICENSE.qcom.txt` and `NOTICE.qcom.txt`, and the image copies
them to `/usr/share/licenses/linux-firmware/`. In particular, the license
restricts the binary firmware to platforms incorporating Qualcomm chipsets;
read the enclosed terms before redistributing or using an image.

The `rootfs/` directory mirrors the Scarlet ABI filesystem layout. The CoachZ
image copies it under `/system/scarlet` in both initramfs and rootfs. The kernel
driver reads the initramfs copy through the global VFS, while Scarlet tasks see
the persistent rootfs copy through the normal `/lib` and `/usr` ABI aliases.
The provenance and expected hashes are also installed in `SOURCE.md`.
