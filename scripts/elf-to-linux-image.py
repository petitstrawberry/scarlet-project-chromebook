#!/usr/bin/env python3
"""Convert Scarlet's linked ELF into an exact Linux arm64 Image."""

from __future__ import annotations

import argparse
import os
import struct
import subprocess
import tempfile
from pathlib import Path

ARM64_MAGIC = 0x644D5241
HEADER_SIZE = 64


def convert(elf: Path, output: Path, objcopy: str) -> dict[str, int]:
    """Extract the load image, discard host metadata, and materialize NOLOAD space."""
    output.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary_name = tempfile.mkstemp(prefix=f".{output.name}.", dir=output.parent)
    os.close(fd)
    temporary = Path(temporary_name)

    try:
        # cargo-scarlet appends an allocated, VMA-zero .ksym sidecar after link.
        # It is host/debugger metadata, outside the kernel's PT_LOAD range. Without
        # removing it, objcopy creates a roughly 1 GiB hole before the real Image.
        subprocess.run(
            [
                objcopy,
                "--remove-section=.ksym",
                "-O",
                "binary",
                str(elf),
                str(temporary),
            ],
            check=True,
        )

        data = temporary.read_bytes()
        if len(data) < HEADER_SIZE:
            raise ValueError("objcopy output is smaller than the arm64 Image header")

        magic = struct.unpack_from("<I", data, 56)[0]
        if magic != ARM64_MAGIC:
            raise ValueError(
                f"objcopy output does not begin at the arm64 Image header: {magic:#x}"
            )

        image_size = struct.unpack_from("<Q", data, 16)[0]
        if image_size < len(data):
            raise ValueError(
                f"linked image_size {image_size:#x} is smaller than file data "
                f"{len(data):#x}"
            )

        # Linux reserves image_size bytes. Materialize NOLOAD BSS, the early page
        # table pool, and the FDT relocation buffer so direct loaders and vboot
        # packers receive one self-contained payload with deterministic contents.
        with temporary.open("ab") as stream:
            stream.truncate(image_size)

        os.replace(temporary, output)
        return {"image_size": image_size, "initialized_size": len(data)}
    finally:
        temporary.unlink(missing_ok=True)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("elf", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--objcopy", default="llvm-objcopy")
    args = parser.parse_args()
    fields = convert(args.elf, args.output, args.objcopy)
    print(
        f"Linux arm64 Image: wrote {fields['image_size']:#x} bytes "
        f"({fields['initialized_size']:#x} initialized)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
