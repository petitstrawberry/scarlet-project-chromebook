# Chromebook boot architecture

## Stable boundary

kernel と host packer の境界は standard Linux arm64 `Image` だけです。

```text
Scarlet ELF
  -> flat arm64 Image (64-byte header, code/data/BSS reservation)
  -> ChromeOS keyblock + kernel preamble + signed body
  -> Depthcharge verifies and places body at body_load_address
  -> x0 = physical DTB, MMU off, EL2 or EL1
  -> Scarlet linux-boot head
  -> identity map + HHDM
  -> BootInfo -> start_kernel -> runtime page table
```

Scarlet kernel は keyblock、preamble、GPT attribute、署名鍵を解釈しません。packer は
Scarlet の page table や `BootInfo` layout を知りません。この分離により、QEMU の
Linux loader と Depthcharge の双方で同一 Image を使えます。

## Implemented kernel path (`../Scarlet`)

`kernel/src/arch/aarch64/boot/linux.rs` は次を実装しています。

1. Linux arm64 header と最初の branch (`text_offset=2 MiB`, LE, 4 KiB granule)。
2. x0 DTB の保存、exception mask、EL2 なら EL1h への降下。
3. initialized data 内の 64 KiB physical boot stack。
4. FDT header/size、RAM nodes、memreserve、`/reserved-memory`、initrd の解析。
5. 物理リンク kernel を継続実行する Normal/executable identity map。
6. 全 RAM の Normal/NX HHDM と、FDT-selected PL011 の Device/NX HHDM。
7. allocator 非依存の固定 page-table pool と EL1 translation-register 設定。
8. linker 内 2 MiB buffer への FDT relocation と initramfs relocation。
9. sparse direct-map metadata、usable RAM、cmdline を持つ `BootInfo`。
10. common `start_kernel` と allocator-backed runtime page table への移行。

kernel image、FDT memreserve、initrd、reserved-memory child は PMM 候補から除外し、残る
page-aligned gap の最大領域を使います。初期 table の identity alias は runtime root page
tableへの切替で退役します。

`linux-boot` は Limine response/HHDM/request section を参照しません。Limine crate 自体も
optional dependency であり、project-generated `scarlet-modules` は `default-features=false`
でビルドされます。

### Deliberate current constraints

- Image は non-relocatable physical link。QEMU BSP は `0x40200000` 固定。
- kernel の virtual base も起動時はその低位 address。HHDM は別に構築される。
- CPU count は 1。PSCI secondary boot hook は未実装。
- early console は FDT の exact `arm,pl011` compatible のみ。
- FDT は最大 2 MiB、early map/reservation は固定容量。
- runtime と early translation regime は 40-bit physical address を前提とする。
- PMM は一つの contiguous usable area を受け取るため、最大 gap を選ぶ。
- current Scarlet heap は 512 MiB contiguous reservation を要求するため QEMU smoke は
  2 GiB RAM を使う。

これらは一般的な Linux register/header ABI と、board policy を分離するための次の作業点
です。実機 BSP で QEMU の絶対 address を流用してはいけません。

## Implemented host path (this repository)

`elf-to-linux-image.py` は post-link `.ksym` metadata を除き、ELF load image を raw binary
へ変換し、header の `image_size` までゼロで materialize します。これにより boot BSS、
early table pool、FDT reserve を含む一つの self-contained Image になります。

`pack-vboot-kernel.py` は pinned official `futility` に署名を委任します。独立 parser は
次を bounds-check します。

- `CHROMEOS` magic、keyblock 2.1 fixed header/data key/hash/signature
- signature-relative offsets と signed-data lengths
- kernel preamble 2.0/2.1/2.2 の版別 fixed size
- preamble/body signature material、signed body range
- body load/bootloader addresses、4 KiB config、4 KiB params、bootloader range

暗号学的な検証は official tool の public-key verify に任せ、独立 parser は構造破壊や
integer/range error を別経路で検出します。packer は signed body の先頭と入力 Image の
完全一致も要求します。private key/keyblock/verification key は repository 外のみです。

## Validation and deployment order

1. host で Image header を検証する。
2. QEMU `-kernel` で Limine なしに起動する。
3. official `futility` と independent parser の両方で vboot blob を検証する。
4. 対象機 recovery kernel partition を read-only で解析し fixture 化する。
5. board-specific load address/FDT/UART/PSCI を実装する。
6. developer key blob を removable/recoverable medium から起動し AP UART を採取する。
7. 最後に GPT image composition と attribute management を追加する。

現時点では in-place GPT mutation、flash、firmware write を提供しません。
