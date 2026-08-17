# AArch64 Linux Image / vboot smoke BSP

Limine を使わず、standard Linux arm64 Image ABI から Scarlet を起動する QEMU
`virt` harness です。`scarlet.toml` は sibling `../../../Scarlet/kernel` を
`default-features = false` 相当で取り込み、`linux-boot` feature を有効にします。

repository root の dev shell から実行してください。

```sh
./scripts/build-linux-image.sh
./scripts/run-linux-qemu-smoke.py --timeout 12
```

linker は Image を `0x40200000` に固定配置します。これは QEMU RAM base
`0x40000000 + text_offset 0x200000` に合わせた検証値で、任意 Chromebook の値では
ありません。vboot pack 時の `--kloadaddr` も同じ値にする必要があります。

現在は単一 CPU、PL011、空の newc initramfs が対象です。実機 flash image ではなく、
Linux ABI、初期 page table/HHDM、vboot body preservation を検証する BSP です。
