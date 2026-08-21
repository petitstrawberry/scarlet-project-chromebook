# Chromebook bring-up roadmap

## 1. Debug transport — complete

- Nix の `python3.withPackages` による PyUSB + libusb shell
- pinned ChromiumOS EC checkout
- Cr50/AP/EC interface wrapper と backend diagnostics

## 2. Linux arm64 boot protocol — QEMU milestone complete

`../Scarlet` に `linux-boot` feature を追加済みです。Linux Image header、x0 DTB、
EL2→EL1、64 KiB boot stack、FDT memory/reservation parsing、初期 identity/HHDM page
tables、runtime `BootInfo` handoff が実装され、QEMU で scheduler/initramfs まで通ります。
Limine dependency は optional で、この BSP の feature graphには入りません。

次の kernel milestone:

- 実機 DRAM map に対応する board BSP と load-address contract
- `/cpus` と PSCI `enable-method` に基づく secondary CPU 起動
- PL011 以外の実機 early UART、または board hook
- boot CPU MPIDR から logical CPU topology への変換
- 物理固定リンクから relocatable Image / high-half execution への発展
- 複数の usable RAM gap を PMM に渡す interface（現在は最大の一領域を選択）

## 3. Verified Boot pack/inspection — host workflow complete

- pinned official `futility` を Linux と Apple Silicon で build
- external key material の強制
- official pack + public-key verify
- keyblock/preamble/body の independent bounded parser
- 入力 Image の signed-body prefix 一致確認
- SHA-256 provenance manifest

次の analysis milestone:

- 対象機 recovery image から kernel partition fixture を取得して比較
- 対象 Depthcharge の arm64 handoff、load address、cmdline/initrd 方針を確定
- production/developer/recovery key policy と rollback version 運用を定義
- malformed/fuzz corpus と cryptographic negative tests を追加

## 4. Real Chromebook image — U-Boot/Limine/Scarlet handoff complete

- `aarch64-coachz-limine` board project and CoachZ Limine ESP scaffold
- patched Trogdor U-Boot `u-boot.elf` build with CoachZ rev3 DT
- `bootcmd=bootflow scan -b`, `bootdelay=0` automatic EFI bootflow selection
- board-specific Scarlet project/BSP
- AP UARTでDepthcharge→U-Boot→USB EFI→Limine→Scarletを実機確認
- Scarlet標準base/cli-utils bundleを含むbootable initramfs

次の実機milestone:

- init→stemd→login/shellの実機確認
- SC7180 USB/MMC block driverと永続rootfs
- ChromeOS kernel GPT partition の priority/tries/successful 属性 parser
- まず read-only inspect、次に explicit output file への image composition

GPT/実デバイスへの in-place write と flash helper は、fixture verification と実機 boot が
成立するまで追加しません。

## 5. cargo-scarlet integration — future

形式が安定したら `cargo-scarlet-plugin-chromeos-vboot` として image format を実装します。
`.scarlet/` を手作業で変更したり、Limine/UEFI image を vboot と見なしたりしません。
詳細な ownership は [docs/boot-architecture.md](docs/boot-architecture.md) を参照してください。
