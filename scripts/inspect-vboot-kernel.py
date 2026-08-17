#!/usr/bin/env python3
"""Bounds-check and summarize a ChromeOS vboot kernel partition."""

from __future__ import annotations

import argparse
import json
import struct
from pathlib import Path

KEYBLOCK_MAGIC = b"CHROMEOS"
KEYBLOCK_FIXED_SIZE = 112
PREAMBLE_20_SIZE = 96
PREAMBLE_21_SIZE = 112
PREAMBLE_22_SIZE = 116
SIGNATURE_SIZE = 24
CROS_CONFIG_SIZE = 4096
CROS_PARAMS_SIZE = 4096


def u32(data: bytes, offset: int) -> int:
    return struct.unpack_from("<I", data, offset)[0]


def u64(data: bytes, offset: int) -> int:
    return struct.unpack_from("<Q", data, offset)[0]


def checked_range(total: int, offset: int, size: int, label: str) -> tuple[int, int]:
    if offset < 0 or size < 0 or offset > total or size > total - offset:
        raise ValueError(
            f"{label} range {offset:#x}+{size:#x} exceeds file size {total:#x}"
        )
    return offset, offset + size


def relative_range(
    data: bytes, signature_offset: int, label: str
) -> tuple[int, int]:
    checked_range(len(data), signature_offset, SIGNATURE_SIZE, f"{label} header")
    data_offset = u32(data, signature_offset)
    data_size = u32(data, signature_offset + 8)
    absolute_offset = signature_offset + data_offset
    return checked_range(len(data), absolute_offset, data_size, label)


def require_nonoverlap(
    ranges: list[tuple[str, tuple[int, int]]]
) -> None:
    ordered = sorted(ranges, key=lambda item: item[1][0])
    for (left_name, left), (right_name, right) in zip(ordered, ordered[1:]):
        if left[1] > right[0]:
            raise ValueError(f"{left_name} overlaps {right_name}")


def inspect(path: Path) -> dict[str, int | str]:
    data = path.read_bytes()
    if len(data) < KEYBLOCK_FIXED_SIZE or data[:8] != KEYBLOCK_MAGIC:
        raise ValueError("not a ChromeOS vboot keyblock (missing CHROMEOS magic)")

    # vb2_keyblock is a packed little-endian structure. The fixed prefix is
    # followed by its data key and signature material; keyblock_size points to
    # the kernel preamble.
    keyblock_version_major = u32(data, 8)
    keyblock_version_minor = u32(data, 12)
    if (keyblock_version_major, keyblock_version_minor) != (2, 1):
        raise ValueError(
            "unsupported keyblock version "
            f"{keyblock_version_major}.{keyblock_version_minor}"
        )
    keyblock_size = u32(data, 16)
    if keyblock_size < KEYBLOCK_FIXED_SIZE:
        raise ValueError(f"keyblock_size is too small: {keyblock_size:#x}")
    checked_range(len(data), 0, keyblock_size, "keyblock")
    keyblock_signature = relative_range(data, 24, "keyblock signature")
    keyblock_hash = relative_range(data, 48, "keyblock hash")
    if keyblock_signature[1] > keyblock_size or keyblock_hash[1] > keyblock_size:
        raise ValueError("keyblock signature material escapes keyblock_size")
    data_key_offset = 80 + u32(data, 80)
    data_key_size = u32(data, 88)
    data_key = checked_range(
        keyblock_size, data_key_offset, data_key_size, "keyblock data key"
    )
    keyblock_signed_size = u32(data, 24 + 16)
    keyblock_hashed_size = u32(data, 48 + 16)
    expected_keyblock_data_end = data_key[1]
    if keyblock_signed_size != expected_keyblock_data_end:
        raise ValueError("keyblock signature covers an unexpected byte range")
    if keyblock_hashed_size != expected_keyblock_data_end:
        raise ValueError("keyblock hash covers an unexpected byte range")
    require_nonoverlap(
        [
            ("keyblock fixed header", (0, KEYBLOCK_FIXED_SIZE)),
            ("keyblock data key", data_key),
            ("keyblock hash", keyblock_hash),
            ("keyblock signature", keyblock_signature),
        ]
    )

    preamble_offset = keyblock_size
    checked_range(len(data), preamble_offset, PREAMBLE_20_SIZE, "kernel preamble")
    preamble_size = u32(data, preamble_offset)
    if preamble_size < PREAMBLE_20_SIZE:
        raise ValueError(f"preamble_size is too small: {preamble_size:#x}")
    checked_range(len(data), preamble_offset, preamble_size, "kernel preamble")

    header_major = u32(data, preamble_offset + 32)
    header_minor = u32(data, preamble_offset + 36)
    if header_major != 2 or header_minor > 2:
        raise ValueError(
            f"unsupported kernel preamble version {header_major}.{header_minor}"
        )
    minimum_preamble_size = (
        PREAMBLE_22_SIZE
        if header_minor == 2
        else PREAMBLE_21_SIZE
        if header_minor == 1
        else PREAMBLE_20_SIZE
    )
    if preamble_size < minimum_preamble_size:
        raise ValueError(
            f"preamble {header_major}.{header_minor} is shorter than its fixed "
            f"header: {preamble_size:#x} < {minimum_preamble_size:#x}"
        )
    kernel_version = u32(data, preamble_offset + 40)
    body_load_address = u64(data, preamble_offset + 48)
    bootloader_address = u64(data, preamble_offset + 56)
    bootloader_size = u32(data, preamble_offset + 64)

    preamble_signature = relative_range(
        data, preamble_offset + 8, "preamble signature"
    )
    body_signature_offset = preamble_offset + 72
    body_signature = relative_range(data, body_signature_offset, "body signature")
    preamble_end = preamble_offset + preamble_size
    if preamble_signature[1] > preamble_end or body_signature[1] > preamble_end:
        raise ValueError("preamble signature material escapes preamble_size")
    preamble_signed_size = u32(data, preamble_offset + 8 + 16)
    if preamble_signed_size < minimum_preamble_size:
        raise ValueError("preamble signature does not cover its fixed header")
    preamble_signed_end = preamble_offset + preamble_signed_size
    if body_signature[1] > preamble_signed_end:
        raise ValueError("preamble signature does not cover the body signature")
    if preamble_signed_end > preamble_signature[0]:
        raise ValueError("preamble signed bytes overlap its signature material")
    if body_signature[1] > preamble_signature[0]:
        raise ValueError("body signature overlaps preamble signature material")
    body_size = u32(data, body_signature_offset + 16)
    body_offset = preamble_offset + preamble_size
    checked_range(len(data), body_offset, body_size, "signed kernel body")
    if bootloader_address < body_load_address:
        raise ValueError("bootloader address precedes the signed kernel body")
    bootloader_offset = bootloader_address - body_load_address
    checked_range(body_size, bootloader_offset, bootloader_size, "bootloader")
    if bootloader_offset < CROS_CONFIG_SIZE + CROS_PARAMS_SIZE:
        raise ValueError("kernel body does not leave room for config and params")
    config_offset = bootloader_offset - CROS_PARAMS_SIZE - CROS_CONFIG_SIZE
    params_offset = config_offset + CROS_CONFIG_SIZE
    checked_range(body_size, config_offset, CROS_CONFIG_SIZE, "kernel config")
    checked_range(body_size, params_offset, CROS_PARAMS_SIZE, "kernel params")

    return {
        "format": "chromeos-vboot-kernel",
        "file_size": len(data),
        "keyblock_version_major": keyblock_version_major,
        "keyblock_version_minor": keyblock_version_minor,
        "keyblock_size": keyblock_size,
        "preamble_offset": preamble_offset,
        "preamble_size": preamble_size,
        "preamble_version_major": header_major,
        "preamble_version_minor": header_minor,
        "kernel_version": kernel_version,
        "body_load_address": body_load_address,
        "bootloader_address": bootloader_address,
        "bootloader_size": bootloader_size,
        "config_offset": config_offset,
        "config_size": CROS_CONFIG_SIZE,
        "params_offset": params_offset,
        "params_size": CROS_PARAMS_SIZE,
        "keyblock_signature_offset": keyblock_signature[0],
        "keyblock_signature_size": keyblock_signature[1] - keyblock_signature[0],
        "keyblock_hash_offset": keyblock_hash[0],
        "keyblock_hash_size": keyblock_hash[1] - keyblock_hash[0],
        "data_key_offset": data_key[0],
        "data_key_size": data_key[1] - data_key[0],
        "preamble_signature_offset": preamble_signature[0],
        "preamble_signature_size": preamble_signature[1] - preamble_signature[0],
        "body_signature_offset": body_signature[0],
        "body_signature_size": body_signature[1] - body_signature[0],
        "body_offset": body_offset,
        "body_size": body_size,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("kernel_partition", type=Path)
    args = parser.parse_args()
    print(json.dumps(inspect(args.kernel_partition), indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
