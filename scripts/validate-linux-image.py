#!/usr/bin/env python3
"""Validate Scarlet's Linux arm64 Image header."""

from __future__ import annotations

import argparse
import struct
from pathlib import Path


def validate(path: Path) -> dict[str, int]:
    data = path.read_bytes()
    if len(data) < 64:
        raise ValueError("Image is smaller than the 64-byte arm64 header")

    _, _, text_offset, image_size, flags, res2, res3, res4, magic, res5 = (
        struct.unpack_from("<IIQQQQQQII", data)
    )
    if magic != 0x644D5241:
        raise ValueError(f"bad arm64 Image magic: {magic:#x}")
    if text_offset != 0x200000:
        raise ValueError(f"unexpected text_offset: {text_offset:#x}")
    if image_size == 0 or image_size != len(data):
        raise ValueError(
            f"invalid image_size {image_size:#x} for {len(data):#x}-byte file"
        )
    if flags & 0b111 != 0b010:
        raise ValueError(f"Image does not declare LE/4K pages: flags={flags:#x}")
    if flags & (1 << 3):
        raise ValueError("physical-link Image must keep the placement flag clear")
    if any((res2, res3, res4, res5)):
        raise ValueError("reserved arm64 Image header fields are non-zero")

    return {
        "text_offset": text_offset,
        "image_size": image_size,
        "flags": flags,
        "file_size": len(data),
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("image", type=Path)
    args = parser.parse_args()
    fields = validate(args.image)
    print(
        "Linux arm64 Image: OK "
        f"(text_offset={fields['text_offset']:#x}, "
        f"image_size={fields['image_size']:#x}, flags={fields['flags']:#x})"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
