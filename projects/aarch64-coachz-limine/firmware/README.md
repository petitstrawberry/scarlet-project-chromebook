# CoachZ Qualcomm firmware

This directory vendors the binary firmware files required by the Adreno A618
GPU and Venus 5.4 video decoder. They are imported byte-for-byte from the
official linux-firmware repository at the pinned revision below; image creation
performs no download.

Upstream repository: <https://gitlab.com/kernel-firmware/linux-firmware>

Pinned revision: `8c7fac62c0d1c3b8915f596effc1ef6e95fd6b5f`

| Upstream file | SHA-256 | Image path |
| --- | --- | --- |
| `qcom/a630_gmu.bin` | `da8d9b1b1f5c1a0b311f32567093b4828f3c80031dd8435f91ac13c664e173a6` | `/lib/firmware/qcom/a630_gmu.bin` |
| `qcom/a630_sqe.fw` | `1c21b527d9183487cc550dabbb3f43e555df5a977a461934fc61f0635a9aa90c` | `/lib/firmware/qcom/a630_sqe.fw` |
| `qcom/venus-5.4/venus.mbn` | `db2a2efefa7149519b36f01be8a7c37bdf21efd2f06eff5028f80d4295aef7d9` | `/lib/firmware/qcom/venus-5.4/venus.mbn` |

These files are not distributed under the repository's GPL license. The
linux-firmware `WHENCE` entry identifies them as redistributable under QTI's
separate terms. The complete upstream `LICENSE.qcom` and `NOTICE.qcom` files
are preserved in the common layer as `LICENSE.qcom.txt` and
`NOTICE.qcom.txt`; the image copies them to
`/usr/share/licenses/linux-firmware/`. In particular, the license
restricts the binary firmware to platforms incorporating Qualcomm chipsets;
read the enclosed terms before redistributing or using an image.

The `qcom/` directory contains only the firmware binaries. Both images copy it
directly to `/lib/firmware/qcom`; kernel drivers read that path from the
initramfs through the global VFS.

The license, notice and `SOURCE.md` payloads live in
[`../layers/common/usr/share/licenses/linux-firmware/`](../layers/common/usr/share/licenses/linux-firmware/)
and are installed into both images. Board settings and arbitrary additional
files belong in [`../layers/rootfs/`](../layers/rootfs/), not this directory.
