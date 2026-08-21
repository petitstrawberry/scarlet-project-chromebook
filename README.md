# Scarlet on Chromebook

This repository contains the board integration, build environment, and host
tools used to boot [Scarlet](https://github.com/petitstrawberry/Scarlet) on a
Chromebook.

The tested target is **Google CoachZ rev3**, a Qualcomm SC7180 device in the
ChromeOS Trogdor family. The project is experimental, but the complete boot
path reaches an interactive Scarlet shell on real hardware.

```text
Qualcomm firmware
  -> coreboot
  -> Depthcharge
  -> RW_LEGACY / alternate firmware
  -> U-Boot
  -> Limine EFI
  -> Scarlet kernel and initramfs
```

## Current status

Working on CoachZ rev3:

- ChromiumOS CCD USB serial access to the GSC, AP, and EC consoles
- U-Boot as a Depthcharge alternate-firmware payload
- automatic EFI bootflow discovery from USB storage
- a patched AArch64 Limine loader with the required MPIDR affinity fix
- Scarlet kernel boot through Limine
- Qualcomm GENI UART output and interrupt-driven input
- Scarlet initramfs, `stemd`, login, and an interactive shell

Still under development:

- framebuffer and display output
- Scarlet-side SC7180 USB host and USB mass-storage support
- persistent USB root filesystem
- SD/eMMC support
- onboard networking and GPU support

The current Scarlet system runs entirely from its initramfs after boot. U-Boot
can read the USB boot medium, but Scarlet does not yet mount a persistent
root filesystem from it.

## Requirements

### Host

- Nix with flakes enabled
- one of the supported hosts:
  - Apple Silicon macOS
  - AArch64 Linux
  - x86-64 Linux

### Chromebook

- Google CoachZ rev3 is the currently tested model
- Developer Mode enabled
- alternate-firmware boot enabled
- a working RW_LEGACY U-Boot installation
- CCD access is strongly recommended for the AP UART and firmware recovery
- a disposable USB drive for the Scarlet boot image

Firmware modification can make the device unbootable. Keep a verified SPI
flash backup before changing RW_LEGACY. This repository builds firmware
artifacts but never invokes `flashrom` automatically.

## Set up the development environment

Clone this project and enter its development shell:

```sh
git clone https://github.com/petitstrawberry/scarlet-project-chromebook.git
cd scarlet-project-chromebook
nix develop --accept-flake-config
```

The development shell provides the pinned Scarlet Rust toolchain,
`cargo-scarlet`, the Limine image plugin, AArch64 build tools, PyUSB/libusb,
device-tree tools, `mtools`, and serial-console utilities.

The Scarlet kernel and reusable filesystem bundles are fetched from the
Scarlet `dev` branch through cargo-scarlet. A sibling Scarlet checkout is not
required.

With direnv installed, the checked-in `.envrc` can enter the same environment:

```sh
direnv allow
```

## Open the AP serial console

Connect CCD, enter the Nix development shell, and verify that PyUSB can load
libusb and see the GSC USB device:

```sh
./scripts/check-pyusb-libusb.py --list
```

For a quick read-only console session:

```sh
./scripts/ec-usb-console.sh ap
```

For interactive use, expose the USB console as a PTY and attach minicom. Run
the bridge in the first terminal:

```sh
./scripts/ec-usb-pty.py ap \
  --raw-log work/uart/coachz-ap.raw
```

Then attach from a second terminal:

```sh
minicom -o -D /tmp/coachz-ap-uart -b 115200
```

The PTY bridge forwards input, including `Ctrl-C`, to the Chromebook. It also
normalizes line endings for terminal display while preserving the exact USB
receive stream in the optional raw log.

The available CCD console targets are:

| Target | USB interface | Console |
| --- | ---: | --- |
| `cr50` | 0 | GSC / Cr50 |
| `ap` | 1 | application processor |
| `ec` | 2 | embedded controller |

The helpers default to USB device `18d1:5014`. Use `--serial` when multiple
devices are attached, or `--device VID:PID` for another compatible GSC.

## Build the CoachZ U-Boot payload

The CoachZ U-Boot work is maintained separately so this repository does not
vendor a complete U-Boot source tree. Prepare the ignored checkout once:

```sh
mkdir -p .cache
git clone --depth 1 \
  --branch wip/coachz-altfw \
  https://github.com/petitstrawberry/u-boot-coachz.git \
  .cache/u-boot
```

Build the alternate-firmware payload from the Nix development shell:

```sh
./scripts/build-u-boot-coachz.sh
```

The resulting payload is:

```text
.cache/u-boot/build-coachz/u-boot.elf
```

It is configured for CoachZ rev3, AP debug UART, EFI bootflow, and automatic
boot with:

```text
cls; bootflow scan -b
```

Installing the payload modifies the Chromebook's SPI flash and is deliberately
manual. Read the [CoachZ U-Boot installation notes](projects/aarch64-coachz-limine/uboot/README.md)
and the fork's
[Trogdor alternate-firmware procedure](https://github.com/petitstrawberry/u-boot-coachz/blob/wip/coachz-altfw/doc/board/google/chromebook_trogdor.rst)
before writing RW_LEGACY.

## Build the Scarlet USB image

From the Nix development shell:

```sh
./scripts/build-coachz-limine-image.sh
```

The release images are written under:

```text
projects/aarch64-coachz-limine/.scarlet/images/
```

The image to deploy to a USB drive is:

```text
projects/aarch64-coachz-limine/.scarlet/images/scarlet-aarch64-coachz-full.img
```

It is a GPT disk image for a single USB drive with:

- partition 1, `SCARLET_BOOT`: an EFI System Partition containing:
  - `EFI/BOOT/BOOTAA64.EFI`
  - the Scarlet AArch64 kernel
  - the Scarlet initramfs built from the `base` and `cli-utils` bundles
  - the generated Limine configuration
- partition 2, `SCARLET_ROOT`: an ext2 filesystem containing the `full` bundle

The kernel command line selects the second partition as the root filesystem:

```text
root=/dev/usbblk0p2 rootfstype=ext2 rootwait
```

The project-local PID 1 honors bare `rootwait`: it waits indefinitely for the
configured root device to become mountable, sleeping one second between
attempts and rate-limiting diagnostics to once every 30 seconds. Once mounted,
it continues through Scarlet's existing root transition. Without bare
`rootwait`, Scarlet retains its single mount attempt and tmpfs fallback.

The standalone ESP image remains available as an intermediate/debug artifact:

```text
projects/aarch64-coachz-limine/.scarlet/images/esp-aarch64-coachz.img
```

It is not the image to write to the whole USB drive for normal deployment.

The project post-image hook automatically replaces the stock Limine loader
with the pinned CoachZ-compatible build, adds the CoachZ Scarlet OS handoff
DTB to the EFI partition, and verifies the injected files. U-Boot continues to
use its USB2-only CoachZ control DTB; the external OS DTB supplied to Scarlet
enables USB3 after handoff. The same hook runs when invoking `cargo scarlet
image` directly, but the wrapper above is the canonical build command.

The combined GPT image, persistent root filesystem, and USB3 OS-DT handoff
have not yet been validated on hardware.

For a debug kernel instead of the default release build:

```sh
./scripts/build-coachz-limine-image.sh --debug
```

## Write the image to USB

The following operation destroys the contents of the selected drive. Verify
the device name before writing.

Using a graphical image writer such as balenaEtcher, select
`scarlet-aarch64-coachz-full.img` and write it to the entire USB drive.

On Linux, the equivalent command is:

```sh
image=projects/aarch64-coachz-limine/.scarlet/images/scarlet-aarch64-coachz-full.img
lsblk
sudo dd if="$image" of=/dev/sdX bs=4M conv=fsync status=progress
```

On macOS:

```sh
image=projects/aarch64-coachz-limine/.scarlet/images/scarlet-aarch64-coachz-full.img
diskutil list
diskutil unmountDisk /dev/diskN
sudo dd if="$image" of=/dev/rdiskN bs=4m
sync
diskutil eject /dev/diskN
```

Replace `/dev/sdX` or `/dev/diskN` with the USB drive, not one of its
partitions and never the host's system disk.

## Boot Scarlet

1. Start the AP UART console.
2. Insert the prepared USB drive into the Chromebook.
3. Reboot to the Developer Mode screen.
4. Select the alternate bootloader and choose U-Boot.
5. U-Boot clears the visible terminal, scans boot media, and starts Limine.

U-Boot uses the USB2-only control DTB during its USB boot scan. Limine then
passes the external OS handoff DTB from the EFI partition to Scarlet, where
the USB3 controller is enabled for the operating system. The combined GPT
image and this persistent-rootfs handoff are awaiting hardware validation.

When the boot path is working, output is expected to look similar to:

```text
limine: Loading executable `boot():/boot/kernel`...
Model: Google CoachZ (rev3+)
Scarlet Shell (Enhanced Interactive Mode)
Welcome to Scarlet Shell!
#
```

If U-Boot reaches its prompt instead of booting automatically, these commands
show whether the USB device and EFI bootflow were discovered:

```text
usb start
bootflow scan -a
```

## Repository layout

```text
flake.nix
  Reproducible host toolchain and pinned external inputs.

projects/aarch64-coachz-limine/
  Scarlet BSP, image manifest, Limine injection hook, and U-Boot notes.

scripts/
  Image builders, U-Boot build helper, and CCD serial tools.

nix/
  Host compatibility files and the pinned Limine AArch64 fix.
```

Generated Scarlet state lives under each project's `.scarlet/` directory.
Upstream source checkouts and local experiments live under `.cache/` and
`work/`. These directories are intentionally ignored by Git.

## Upstream work

This project builds on:

- [Scarlet](https://github.com/petitstrawberry/Scarlet)
- [U-Boot](https://github.com/u-boot/u-boot)
- [CoachZ U-Boot fork](https://github.com/petitstrawberry/u-boot-coachz/tree/wip/coachz-altfw)
- [Trogdor U-Boot patch series](https://lists.denx.de/pipermail/u-boot/2025-May/590415.html)
- [Limine](https://github.com/limine-bootloader/limine)
- [ChromiumOS EC USB console](https://chromium.googlesource.com/chromiumos/platform/ec/+/ab6941cd8b5b973152d6ad947daa43160d8e9d2b/extra/usb_serial/console.py)

## License

This repository is licensed under the GNU General Public License version 2
only. See [LICENSE](LICENSE). Imported or referenced upstream work remains
subject to its original copyright and license terms.
