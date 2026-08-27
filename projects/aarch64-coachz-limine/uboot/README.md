# CoachZ U-Boot payload

This directory documents the firmware handoff for Google CoachZ (Qualcomm
SC7180/Trogdor). The U-Boot tree is intentionally kept in the ignored cache;
the project does not vendor a second copy of the upstream source.

```text
XBL/ABL -> coreboot -> Depthcharge -> RW_LEGACY u-boot.elf
        -> U-Boot EFI bootflow -> EFI/BOOT/BOOTAA64.EFI (Limine)
        -> Scarlet
```

Build the locally prepared tree from the repository root:

```sh
./scripts/build-u-boot-coachz.sh
```

The current cache is based on U-Boot `527115ef6783cec49e5610c523c124b399011361`;
the cache remains ignored so upstream source is not vendored into this project.

The result is `.cache/u-boot/build-coachz/u-boot.elf`. The CoachZ defconfig
uses the upstream Trogdor payload support, the CoachZ rev3 device tree, GENI
UART/SPI, SC7180 clocks/pinctrl, EFI bootflow, and:

```text
bootcmd=cls; bootflow scan -b
bootdelay=0
```

Thus U-Boot clears an ANSI-capable terminal, scans USB/MMC boot devices, and
boots the first valid EFI bootflow without requiring an interactive command.
The clear sequence affects only the visible terminal; raw UART captures retain
the previous boot history. `CONFIG_CMD_CLS=y` and the complete boot command are
compiled into the payload and checked by the build helper.

## Install into RW_LEGACY (manual, hardware-affecting)

Do not run these commands against an unverified firmware dump. On the
Chromebook, enable Developer Mode and alternate firmware, keep a recovery copy,
then use the documented Depthcharge altfw flow:

```sh
flashrom -r bios.bin
cat > ubootfw.txt <<'EOF'
0;uboot;uboot;U-Boot default entry
1;uboot;uboot;U-Boot menu entry
EOF
cbfstool bios.bin remove -r RW_LEGACY -n altfw/list || true
cbfstool bios.bin add -r RW_LEGACY -n altfw/list -f ubootfw.txt -t raw
cbfstool bios.bin add-payload -r RW_LEGACY -c lzma -n uboot \
  -f u-boot.elf
flashrom -w bios.bin -i RW_LEGACY
crossystem dev_boot_altfw=1
crossystem dev_default_boot=altfw
```

The `altfw/list` entry with sequence number `0` is required for the timeout
path: Depthcharge uses it as the default alternate bootloader. Sequence `1`
is the visible menu entry. Keep both lines even when they point to the same
U-Boot payload; otherwise the altfw menu can appear automatically while still
requiring a manual U-Boot selection.

If the list is correct but the firmware still opens the selection screen after
the developer-mode timeout, the installed Depthcharge likely predates the
upstream fix `fd736823` (“Boot from default target altfw after timeout”). That
behavior cannot be changed by U-Boot or `crossystem`; it requires updating the
Depthcharge/main firmware, or accepting one `Enter` keypress in the menu.

The repository never runs `flashrom` or writes firmware automatically. First
boot should be observed over the AP UART (`./scripts/ec-usb-console.sh ap`).
The expected U-Boot command is `cls; bootflow scan -b`; if the EFI image is not
discovered, stop at the prompt and run `usb start` followed by `bootflow scan -a`
to inspect bootdev/method errors before changing the image layout.

The starting point is Stephen Boyd's [Trogdor U-Boot series](https://lists.denx.de/pipermail/u-boot/2025-May/590415.html)
and its [installation procedure](https://lists.denx.de/pipermail/u-boot/2025-May/590443.html),
adapted here to CoachZ's upstream device tree.
