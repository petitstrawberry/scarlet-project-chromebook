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

See [onboard Wi-Fi](docs/wireless.md) for setup, supported modes and driver dependencies.

## Current status

Working on CoachZ rev3:

- ChromiumOS CCD USB serial access to the GSC, AP, and EC consoles
- U-Boot as a Depthcharge alternate-firmware payload
- automatic EFI bootflow discovery from USB storage
- a patched AArch64 Limine loader with the required MPIDR affinity fix
- Scarlet kernel boot through Limine
- Qualcomm GENI UART output and interrupt-driven input
- Scarlet initramfs, `stemd`, login, and an interactive shell
- SC7180 eMMC root storage and removable SD card support (`mmcblk0`/`mmcblk1`)
- onboard WPA2-Personal/CCMP Wi-Fi on 2.4 GHz and 5 GHz, with DHCP/DNS
- native internal display and Adreno GPU composition
- native USB-C DisplayPort external output as an extended SWS display

Still under development:

- per-output shell chrome, workspace presentation and display configuration
- Scarlet-side SC7180 USB host, USB mass-storage, and CDC-NCM support
- persistent USB root filesystem
- further wireless PHY/security modes and GPU features

The full image boots through its initramfs and waits for the USB root
filesystem at `/dev/usbblk0p2`, then mounts that ext2 partition for userspace.
CoachZ publishes its soldered eMMC as `/dev/mmcblk0` and an inserted SD card
as `/dev/mmcblk1`. The SD slot is probed during boot; a card inserted later
needs a reboot until hotplug is implemented.

SDCC uses owned ADMA2 buffers through the firmware-described SMMU stream,
with multi-sector transfers up to 256 KiB (64 KiB if allocation is limited).
The SD slot negotiates High Speed or UHS-I SDR12/25/50/104. UHS requires the
CoachZ PM6150L VMMC/VQMMC rails: CMD11 switches signaling to 1.8 V, and
SDR104 validates CMD19 across the 16 CM DLL phases at the 202 MHz source
clock. Failed UHS initialization power-cycles the SD card and retries at
3.3 V; the eMMC rails retain firmware ownership. SDR104 retunes before I/O
after 60 seconds or a transfer error. Existing SD-card contents are preserved
by read-only validation; hotplug is still pending.

Measure device bandwidth separately from hashing with
`storage-check read-bench /dev/mmcblk1 1024 256` (1 GiB, 256 KiB requests).
Use `storage-check hash-prefix /dev/mmcblk1p1 16` for a repeatable integrity
comparison. The benchmark reports decimal MB/s and does not write the card.

## Boot from the removable SD card

The SD variant loads Limine, the kernel, and initramfs from the card's EFI
partition and mounts `/dev/mmcblk1p2` as its ext2 root filesystem:

```sh
nix develop --accept-flake-config -c ./scripts/build-coachz-limine-image.sh --sd
```

The result is
`projects/aarch64-coachz-limine/.scarlet/images/scarlet-aarch64-coachz-sd.img`.
Its first partition is an EFI system partition and its second partition
contains the full rootfs bundle. Insert the card before boot. U-Boot's existing
EFI bootflow scans MMC devices; the kernel and initramfs use `boot():` paths
so they are read from the same card that supplied Limine.

Init also mounts the card's FAT32 EFI partition (`/dev/mmcblk1p1`) read-write at
`/boot`. The USB variant uses `/dev/usbblk0p1`. These are selected explicitly
by `boot=` and `bootfstype=fat32` in the respective Limine command lines; a
failed boot-partition mount is reported without preventing root startup.
The EFI partition itself contains a `boot/` directory, so the mounted kernel
and initramfs are `/boot/boot/kernel` and `/boot/boot/initramfs-aarch64.cpio`. Replacing those
files updates the next boot without rewriting the whole SD image. Finish file
writes and shut down cleanly before removing the card.

Writing the whole SD image replaces the card's partition table and files.
Confirm the card's identity and back up anything needed before installation.
The generated image initially contains a content-sized rootfs; enlarging the
root partition/filesystem is a separate installation step. The existing
`scarlet-aarch64-coachz-full.img` retains its Pi USB root selector and can be
used for recovery.

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
dependencies stay aligned. The kernel, standard bundles, and driver dependencies
use the Scarlet revision recorded in `projects/aarch64-coachz-limine/scarlet.toml`
and `scarlet.lock`. This tracks Scarlet `dev` at `9d7268d333cae6e524549ce7c7298acc07ebda35`;
Android is excluded from `experimental` and remains an explicit opt-in bundle.
The SDK and Rust toolchain are pinned in `flake.lock`. The Adreno backend uses the bundled SGFX revision
`61e5ddd0f0e31309d2468ace3c4afe67d949fa72`. Release builds disable `sync-debug`,
and the full rootfs has the same 8 GiB minimum as Scarlet's full project.

Adreno implements buffer-to-buffer copies with bounded GPU `CP_MEMCPY` packets,
retains both allocations through asynchronous completion, and emits real CCU
flush/invalidate and CP memory ordering for global resource barriers. Ordinary
ordered-queue transitions advertise SGFX's implicit-barrier capability so the
frontend can omit redundant submissions. Portable tests feed these command
streams through the actual kernel validator, including relocation bounds and
write authority. Native programmable graphics, portable buffer compute, and eight color attachments
are enabled after CoachZ output checks. Other optional capabilities are enabled
only after their corresponding installed-driver checks pass. Programmable NV12
sampling, robust texel loads, gathers, and transparent-border filtering are
verified through decoder-backed WGSL and SPIR-V pixel checks.
The standard `/init` provides USB
`rootwait` and constructs the current boot Environment; no project-local init
replacement is needed.

With direnv installed, the checked-in `.envrc` can enter the same environment:

```sh
direnv allow
```

## Open the AP serial console

For CCD attached to the Pi, use `./scripts/raspi-scarlet-control.sh console ap`
(see [network controller setup](#raspberry-pi-network-controller)). The commands
below apply to CCD attached directly to the development host.

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

The kernel, standard bundles and board drivers use Scarlet
`9d7268d333cae6e524549ce7c7298acc07ebda35`.
The standard bundles provide their pinned
SGFX, native Vulkan and ScarletUI dependencies; the project's SGFX dependency
matches the bundle's `61e5ddd0f0e31309d2468ace3c4afe67d949fa72` revision.
Native clients use the published
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

Board firmware binaries are stored in `projects/aarch64-coachz-limine/firmware/`.
Settings and other filesystem additions use the separate
[file layers](projects/aarch64-coachz-limine/layers/README.md): `layers/common/`
and `layers/rootfs/` are tracked, while `layers/private/common/` and
`layers/private/rootfs/` are ignored by Git and applied last in local builds.

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

## Raspberry Pi network controller

Full images require at least 8 GiB for the root filesystem. When the Pi's
`/run` tmpfs is smaller, set `SCARLET_CURRENT_IMAGE=/var/lib/scarlet/current.img`
in `/etc/default/scarlet-gadget` and create `/var/lib/scarlet` before deploying.
This stores the backing image on disk and preserves it across Pi reboots.

The optional Raspberry Pi 5 helper provides USB storage, networking and CCD
control over SSH. Connect the Pi's Ethernet or Wi-Fi to the LAN, its USB-C
gadget port to the Chromebook, and CCD to a Pi USB host port. The default
USB functions are `mass_storage,ncm`; EC/CCD injects keyboard shortcuts, so
the Pi-side HID function stays disabled.

Client scripts share SSH settings. Explicit environment values take
precedence over the optional local file `~/.config/scarlet/raspi.env` (or
`SCARLET_RPI_CLIENT_CONFIG`). For example:

```sh
SCARLET_RPI_HOST=usb-netboot.local
SCARLET_RPI_USER=pi
# Optional; otherwise SSH uses its normal agent/configuration:
# SCARLET_RPI_IDENTITY=/path/to/private/key
```

Without a configured host, scripts use `raspberrypi.local`. Keep machine
addresses and key paths in this local file instead of editing tracked scripts.

Install or refresh the Pi-side services, then deploy an image:

```sh
./scripts/install-raspi-scarlet.sh
./scripts/deploy-scarlet-to-raspi.sh
./scripts/raspi-scarlet-control.sh status
```

The installer preserves `/etc/default/scarlet-gadget` when it already exists.
Set `SCARLET_RPI_REPLACE_CONFIG=1` to replace it with the checked-in defaults;
the previous configuration is kept as `/etc/default/scarlet-gadget.previous`.
When Scarlet is already using USB storage, refresh the services without
rebinding the active gadget:

```sh
SCARLET_RPI_RESTART_GADGET=0 ./scripts/install-raspi-scarlet.sh
```

For a Pi using the old direct Ethernet profile, stage a LAN DHCP profile:

```sh
./scripts/configure-raspi-lan.sh
```

This keeps the current connection up. Reconnect Ethernet to the router to
activate DHCP, then use the Pi's `<hostname>.local` address. The helper
requires NetworkManager and enables mDNS through Avahi.

The Pi-side defaults live in `scripts/raspi-scarlet-gadget.env`:

```sh
SCARLET_GADGET_FUNCTIONS=mass_storage,ncm
SCARLET_NCM_PI_ADDRESS=192.168.88.1/30
SCARLET_NCM_UPLINK_IFNAME=auto
SCARLET_NCM_DNS_PROXY=1
```

The `scarlet-ncm.service` advertises `192.168.88.2` over DHCP on `usb0`.
Scarlet sees `usbnet0` and uses the Pi as its gateway and DNS resolver.
Forwarding and NAT follow the Pi's routing decisions, including Ethernet or
Wi-Fi changes. Set an explicit `SCARLET_NCM_UPLINK_IFNAME` to restrict the
uplink; an explicit `SCARLET_NCM_UPLINK_GATEWAY` requires an explicit
interface. `SCARLET_NCM_DHCP_DNS` can select comma-separated upstream IPv4
resolvers. Set `SCARLET_NCM_CONFIGURE_NETWORK=0` when another manager owns
`usb0`, or `SCARLET_NCM_ENABLE_NAT=0` when the upstream router has a route.

`scarlet-ccd.service` owns the three CCD USB interfaces, reconnects after
unplugging, and shares console output with multiple SSH clients. Its socket
is root-only; the launchers use `sudo -n` on the Pi. Console history retains
the last 64 KiB per target in memory.

```sh
./scripts/raspi-scarlet-control.sh console ap
./scripts/raspi-scarlet-control.sh log ec
./scripts/raspi-scarlet-control.sh send ec version
./scripts/raspi-scarlet-boot.sh
./scripts/raspi-scarlet-boot.sh --delay-ms 12000
```

The boot helper resets only the AP, waits five seconds by default, then
injects `Ctrl+L` through the EC keyboard matrix. It does not power-cycle the
Pi. Control sequences are serialized with console input.

When CCD is attached directly to the Mac, use the existing local helpers:

```sh
./scripts/ec-usb-console.sh --target ap --forward-ctrl-c
./scripts/raspi-scarlet-boot.sh --backend local
```

For a direct Ethernet connection through the Mac, configure an explicit Pi
uplink gateway and enable Mac routing:

```sh
sudo ./scripts/mac-scarlet-routing.sh enable
sudo ./scripts/mac-scarlet-routing.sh disable
```

Use `disable` when the direct-link session ends. A Pi reboot
clears an image stored in `/run/scarlet`, so redeploy it after rebooting when
using that default location.

## Read power information

The primary Chrome EC registers `cros-ec-battery` and `cros-ec-usb` with
Scarlet's read-only `/dev/power_supply` API. Run `power-info` to inspect the
same snapshots used by desktop clients. Battery observations include presence,
charging state, remaining/full capacity ratio, voltage, signed current
(positive into the battery), and temperature when EC command version 1 is
available. Older firmware falls back to version 0 or its battery memory map.
Invalid or unsupported measurements remain absent; an idle battery is not
automatically labeled full.

The USB snapshot describes the EC-selected charging input. A port sourcing
power or connected without supplying the AP is not online. Its current limit
is explicitly a configured limit, not measured current. These providers issue
only read commands and do not alter charging policy, calibration or USB roles.

Portable decoding and EC event-isolation tests:

```sh
cargo test --locked --manifest-path tests/ec-event-drain/Cargo.toml
```

## Validate the installed Adreno driver

The Adreno bundle installs `sgfx-adreno-check`. It reports the actual dynamic
driver's capabilities and verifies ten alternating GPU upload/copy/draw/readback
generations, including a 320 KiB copy across native chunk boundaries, nonzero
offsets, and explicit/implicit copy-to-vertex transitions. Every output pixel
is checked after the completion receipt retires; borrowed uploads are dropped
before waiting. A separate public-ABI test submits and retires the global
resource barrier through the backend ABI.

The check also renders a half-transparent retained tile over an opaque
background and checks every pixel, then exercises replace, additive, erase,
subtract, reverse-subtract, destination-in, and source-color blending. These
cover the premultiplied tiles and fallback backdrop filters used by ScarletUI.
The native encoder and kernel validator accept all SGFX fixed-function blend
factors and operations independently of the canonical shader variant; the
validator still rejects reserved fields and unsupported constant/dual-source
factors. Host tests cover the 500 factor/operation combinations, while the
installed check verifies the listed modes on the GPU, including Min/Max with
zero factors and independent color/alpha operations via the current wire ABI.

Native samplers implement independent minification/magnification filters,
clamp/repeat/mirror/transparent-border addressing, and anisotropy through 16.
Transparent borders use A6xx fast border color; the validator forbids indirect
border-table indices, unsupported flags, and reserved sampling modes. Hardware
checks cover both address axes, all sixteen anisotropy settings, six fixed mip
levels, and a stretched texture footprint where increasing anisotropy retains
more detailed mip levels. Exact implicit LOD selection at primitive edges is
implementation dependent, so that check validates every sampled mip color and
compares the complete image's LOD distribution.

`sgfx-adreno-check --fixed-only` limits the exit criterion to this fixed-pipeline
contract. Without that option, missing extended capabilities cause failure
even if the fixed-pipeline pixel test passes. Capability bits alone do not
validate programmable rendering or compute execution. Run
`sgfx-adreno-check --programmable` for those paths. The NV12-specific selectors
`--nv12-only`, `--nv12-programmable-only`, `--nv12-reconstruction-only`,
`--nv12-border-only`, and `--nv12-slots-only` cover imported two-plane images,
ordinary texture restoration, boundary reconstruction, and all sixteen public
sampler slots. These selectors also take `--programmable`.

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
rather than allowing submissions at an unknown rate.

The default `simple_ondemand` policy uses GPU hardware busy-counter deltas
measured at a stable acknowledged OPP. It sums work across passes in 16.67 ms
windows and estimates pending queue demand against a 10 ms GPU budget. Rising
demand raises the OPP immediately; lowering requires sustained low demand and
steps down one OPP at a time. An idle grace period avoids a clock transition
between every small submission. Thermal limits and explicit manual policies
remain authoritative. Use `devfreqctl governor gpu simple_ondemand` to restore
automatic control after a manual test.

Task Manager reports measured GPU utilization separately from the controller's
work estimate. `devfreqctl get` also exposes failed samples, transition failures,
and transition call durations. Queue wait, CPU command preparation, and display
wait are not GPU busy time; this controller cannot by itself remove those stalls.
