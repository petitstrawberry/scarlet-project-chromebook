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
- Scarlet-side SC7180 USB host, USB mass-storage, and CDC-NCM support
- persistent USB root filesystem
- SD/eMMC support
- onboard networking and GPU support

The full image boots through its initramfs and waits for the USB root
filesystem at `/dev/usbblk0p2`, then mounts that ext2 partition for userspace.

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

The CoachZ kernel and userspace bundles are selected from published Git
revisions by cargo-scarlet. Every board driver uses the same pinned kernel
package; no sibling Scarlet checkout is required. The project-owned Adreno
driver bundle is built from this checkout so its userspace and kernel
dependencies stay aligned. The current kernel includes firmware-managed GPU
frequency registration, with its matching revisions recorded in
`scarlet.toml` and the Cargo lockfiles.
The standard `/init` provides USB
`rootwait` and constructs the current boot Environment; no project-local init
replacement is needed.

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
cargo scarlet image --project projects/aarch64-coachz-limine --release
```

The kernel, standard bundles, board drivers and project-owned Adreno runtime
all reference Scarlet `f8d28abd`. The standard bundles provide their pinned
SGFX, native Vulkan and ScarletUI dependencies; the project's SGFX dependency
matches the bundle's `0ee6b416` revision. Native clients use the published
SGFX backend discovery path. The project-owned Adreno driver is installed at
`/lib/sgfx/libsgfx_scarlet_adreno.so`; `sgfx-probe` reports the selected backend
and linkage. The `sgfx-native` bundle generates the native userspace Cargo
configuration before applications are built.

Userspace is built by the standard Scarlet bundles with their published Git
revisions and checked-in Cargo locks. The project does not inject local source
patches or run a separate userspace rebuild hook. The Adreno shared driver is
built and installed by this repository's `bundles/sgfx-adreno` bundle, using
standard `cargo` (`lib`) and `copy` layers. The SDK records the driver selector
and artifact hash in `scarlet.lock`.

The release images are written under:

```text
projects/aarch64-coachz-limine/.scarlet/images/
```

The image to deploy to a USB drive is:

```text
projects/aarch64-coachz-limine/.scarlet/images/scarlet-aarch64-coachz-full.img
```

`cargo scarlet image` owns image composition and cache reuse. The compatibility
wrapper in `scripts/` only forwards to that command. External sources are
fetched from pinned Git revisions without a sibling Scarlet checkout; the
Adreno bundle is read from this checkout.

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

Scarlet's standard PID 1 honors bare `rootwait`: it waits indefinitely for the
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
cargo scarlet image --project projects/aarch64-coachz-limine
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

## Raspberry Pi USB controller

The optional Raspberry Pi 5 helper presents the Scarlet image as a composite
USB gadget. Its default function set is `mass_storage,ncm`: Scarlet's standard
CDC-NCM host driver registers the network interface as `usbnet0`, while the
image remains a normal `/dev/usbblk0` mass-storage device. Keyboard input is
injected through the Chromebook EC over CCD, so the Pi-side HID function stays
off by default.

Install or refresh the Pi-side gadget and SSH controller:

```sh
./scripts/install-raspi-scarlet.sh
./scripts/deploy-scarlet-to-raspi.sh
./scripts/raspi-scarlet-control.sh status
```

The Pi-side configuration lives in `scripts/raspi-scarlet-gadget.env`:

```sh
SCARLET_GADGET_FUNCTIONS=mass_storage,ncm
SCARLET_NCM_PI_ADDRESS=192.168.88.1/30
```

The Pi configures `usb0` as `192.168.88.1/30` and the companion
`scarlet-ncm.service` advertises `192.168.88.2` over DHCP. The CoachZ image
contains an exact `netcfgd` entry for `usbnet0`, so it installs the Pi as the
default gateway. IPv4 forwarding and an nftables masquerade route the USB
subnet through the Pi's `eth0` (`192.168.77.2`); change
`SCARLET_NCM_UPLINK_IFNAME` when the upstream link is different. DHCP does not
advertise a DNS server by default because this development LAN currently has
no default route; set `SCARLET_NCM_DHCP_DNS` to a comma-separated list when the
upstream can reach one. Set `SCARLET_NCM_CONFIGURE_NETWORK=0` when another
network manager should own `usb0`, or set `SCARLET_NCM_ENABLE_NAT=0` when the
upstream router provides a route instead. Future functions can be enabled by
appending their registry name (for example `mass_storage,ncm,hid`);
unsupported or duplicate names fail before the gadget is rebound.

The Pi's direct Ethernet profile uses `192.168.77.1` as its upstream gateway,
so the route survives NetworkManager restarts. The Mac must also have IP
forwarding/Internet Sharing enabled if traffic should continue past that
direct link; Pi-side NAT alone cannot make a Mac forward packets while
`net.inet.ip.forwarding` is disabled.

For a temporary development session, the repository helper loads only a
volatile PF anchor (it does not edit `/etc/pf.conf`):

```sh
sudo ./scripts/mac-scarlet-routing.sh enable
sudo ./scripts/mac-scarlet-routing.sh disable
```

When updating the Pi while Scarlet is already booted, leave the active USB
image attached with:

```sh
SCARLET_RPI_RESTART_GADGET=0 ./scripts/install-raspi-scarlet.sh
```

This refreshes the DHCP/routing service without interrupting the running
mass-storage session. The normal installer restart is still useful after a
Pi reboot or when switching the image composition.

Run the reset and alternate-firmware shortcut as one sequence:

```sh
./scripts/raspi-scarlet-boot.sh
./scripts/raspi-scarlet-boot.sh --delay-ms 12000
```

The default sequence sends `apreset` over the Mac's CCD EC console, waits
5 seconds, then injects `Ctrl+L` through the EC keyboard matrix. Tune
`--delay-ms` if needed; no firmware image is written by this helper.

The current reset path can remain on the Mac's CCD connection. A later
controller backend can move EC/AP reset to a CCD connection on the Pi without
changing the storage/network function registry. The image is intentionally
kept under `/run/scarlet` (tmpfs), so a Pi reboot requires another image
deployment.

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

## GPU frequency policy

After the GPU initializes, `devfreqctl get` lists the device-tree and speed-bin
validated OPPs for `gpu`. Select an available rate with
`devfreqctl set gpu <kHz>`, or use `devfreqctl governor gpu performance` /
`devfreqctl governor gpu powersave`. The common `/dev/devfreq` interface also
feeds Task Manager. Frequency values are the GMU-acknowledged OPP, not an
independent clock measurement. The GMU applies the matching voltage vote;
changes serialize with GPU submissions. A failed vote marks the GPU lost
rather than allowing submissions at an unknown rate. Utilization-based
governors require hardware counters and are not enabled by this driver.
