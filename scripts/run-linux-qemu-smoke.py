#!/usr/bin/env python3
"""Run Scarlet through QEMU's standard Linux arm64 Image loader."""

from __future__ import annotations

import argparse
import subprocess
import sys
from pathlib import Path

DEFAULT_IMAGE = Path(
    "projects/aarch64-chromebook-smoke/.scarlet/images/Image"
)
DEFAULT_INITRAMFS = Path(
    "projects/aarch64-chromebook-smoke/.scarlet/images/"
    "initramfs-aarch64-chromebook-smoke.cpio"
)
REQUIRED_MARKERS = (
    "[linux-boot] temporary identity/HHDM page table active",
    "[Scarlet Kernel] HHDM offset",
    "[Scarlet Kernel] Heap allocation test passed",
    "[vm] kernel_vm_init: done",
    "PL011 UART device registered",
    "[boot] Initializing scheduler",
)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--image", type=Path, default=DEFAULT_IMAGE)
    parser.add_argument("--initrd", type=Path, default=DEFAULT_INITRAMFS)
    parser.add_argument("--timeout", type=float, default=20.0)
    parser.add_argument("--memory-mib", type=int, default=2048)
    parser.add_argument("--qemu", default="qemu-system-aarch64")
    args = parser.parse_args()

    if args.memory_mib < 2048:
        parser.error(
            "Scarlet currently reserves a 512 MiB physically contiguous initial "
            "heap; the smoke test requires at least 2048 MiB"
        )
    if not args.image.is_file():
        parser.error(f"Image does not exist: {args.image}")
    if not args.initrd.is_file():
        parser.error(f"initramfs does not exist: {args.initrd}")

    command = [
        args.qemu,
        "-machine",
        "virt,gic-version=3",
        "-cpu",
        "cortex-a72",
        "-m",
        str(args.memory_mib),
        "-smp",
        "1",
        "-display",
        "none",
        "-serial",
        "stdio",
        "-monitor",
        "none",
        "-no-reboot",
        "-kernel",
        str(args.image),
        "-initrd",
        str(args.initrd),
    ]
    try:
        completed = subprocess.run(
            command,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            timeout=args.timeout,
            check=False,
        )
        output = completed.stdout
    except subprocess.TimeoutExpired as error:
        output = error.stdout or ""
        if isinstance(output, bytes):
            output = output.decode(errors="replace")

    sys.stdout.write(output)
    missing = [marker for marker in REQUIRED_MARKERS if marker not in output]
    if missing:
        print("QEMU smoke: missing markers:", file=sys.stderr)
        for marker in missing:
            print(f"  - {marker}", file=sys.stderr)
        return 1

    print("QEMU Linux arm64 boot smoke: OK")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
