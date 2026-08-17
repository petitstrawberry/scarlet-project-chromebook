#!/usr/bin/env python3
"""Pack and verify a Scarlet Image using the official ChromeOS vboot tool."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import tempfile
from pathlib import Path

import importlib.util

ROOT = Path(__file__).resolve().parent.parent


def load_script(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load helper: {path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


validate_module = load_script(
    "validate_linux_image", ROOT / "scripts/validate-linux-image.py"
)
inspect_module = load_script(
    "inspect_vboot_kernel", ROOT / "scripts/inspect-vboot-kernel.py"
)


def digest(path: Path) -> str:
    value = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()


def regular_external_file(path: Path, label: str) -> Path:
    resolved = path.expanduser().resolve(strict=True)
    if not resolved.is_file():
        raise ValueError(f"{label} is not a regular file: {resolved}")
    try:
        resolved.relative_to(ROOT)
    except ValueError:
        return resolved
    raise ValueError(f"{label} must stay outside the repository: {resolved}")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--image", type=Path, required=True)
    parser.add_argument("--config", type=Path, required=True)
    parser.add_argument("--keyblock", type=Path, required=True)
    parser.add_argument("--signprivate", type=Path, required=True)
    parser.add_argument("--signpubkey", type=Path, required=True)
    parser.add_argument("--bootloader", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--version", type=int, required=True)
    parser.add_argument("--pad", type=int, default=65536)
    parser.add_argument(
        "--kloadaddr",
        required=True,
        help="physical address where firmware will load the signed kernel body",
    )
    parser.add_argument("--futility", default="futility")
    args = parser.parse_args()

    if args.version < 0 or args.version > 0xFFFF_FFFF:
        parser.error("--version must fit in uint32")
    if args.pad < 0:
        parser.error("--pad must be non-negative")
    try:
        kloadaddr = int(args.kloadaddr, 0)
    except ValueError:
        parser.error("--kloadaddr must be an integer")
    if kloadaddr < 0 or kloadaddr > 0xFFFF_FFFF_FFFF_FFFF:
        parser.error("--kloadaddr must fit in uint64")

    image = args.image.resolve(strict=True)
    config = args.config.resolve(strict=True)
    config_data = config.read_bytes()
    if len(config_data) >= inspect_module.CROS_CONFIG_SIZE:
        parser.error(
            f"--config must be smaller than {inspect_module.CROS_CONFIG_SIZE} "
            "bytes so the vboot field remains NUL-terminated"
        )
    normalized_config = config_data.replace(b"\n", b" ").rstrip(b" ")
    keyblock = regular_external_file(args.keyblock, "keyblock")
    signprivate = regular_external_file(args.signprivate, "private key")
    signpubkey = regular_external_file(args.signpubkey, "public verification key")
    bootloader = (
        args.bootloader.resolve(strict=True) if args.bootloader else None
    )
    validate_module.validate(image)

    futility = shutil.which(args.futility)
    if not futility:
        parser.error(
            "futility is unavailable; enter this repository's nix develop shell "
            "or build .#vboot-tools"
        )
    tool_version = subprocess.run(
        [futility, "version"],
        check=True,
        text=True,
        stdout=subprocess.PIPE,
    ).stdout.strip()

    output = args.output.resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=output.parent) as directory:
        temporary = Path(directory) / output.name
        command = [
            futility,
            "vbutil_kernel",
            "--pack",
            str(temporary),
            "--keyblock",
            str(keyblock),
            "--signprivate",
            str(signprivate),
            "--version",
            str(args.version),
            "--config",
            str(config),
            "--vmlinuz",
            str(image),
            "--arch",
            "arm",
            "--pad",
            str(args.pad),
            "--kloadaddr",
            hex(kloadaddr),
        ]
        if bootloader:
            command.extend(["--bootloader", str(bootloader)])
        subprocess.run(command, check=True)

        verify = [futility, "vbutil_kernel", "--verify", str(temporary)]
        verify.extend(["--signpubkey", str(signpubkey)])
        verified = subprocess.run(
            verify, check=True, text=True, stdout=subprocess.PIPE
        )
        summary = inspect_module.inspect(temporary)
        if summary["body_load_address"] != kloadaddr:
            raise ValueError(
                "official vboot output changed the requested kernel body load "
                f"address: {summary['body_load_address']:#x} != {kloadaddr:#x}"
            )
        body_offset = summary["body_offset"]
        config_offset = summary["config_offset"]
        bootloader_offset = summary["bootloader_address"] - kloadaddr
        with temporary.open("rb") as packed, image.open("rb") as raw_image:
            packed.seek(body_offset)
            if packed.read(image.stat().st_size) != raw_image.read():
                raise ValueError(
                    "official vboot output does not preserve the input Image at "
                    "the start of the signed kernel body"
                )
            image_padding_size = config_offset - image.stat().st_size
            if image_padding_size < 0 or packed.read(image_padding_size).strip(b"\0"):
                raise ValueError("official vboot output has non-zero Image padding")
            packed_config = packed.read(inspect_module.CROS_CONFIG_SIZE)
            expected_config = normalized_config + bytes(
                inspect_module.CROS_CONFIG_SIZE - len(normalized_config)
            )
            if packed_config != expected_config:
                raise ValueError(
                    "official vboot output does not preserve the padded config field"
                )
            if bootloader:
                packed.seek(body_offset + bootloader_offset)
                packed_bootloader = packed.read(summary["bootloader_size"])
                bootloader_data = bootloader.read_bytes()
                expected_bootloader = bootloader_data + bytes(
                    summary["bootloader_size"] - len(bootloader_data)
                )
                if packed_bootloader != expected_bootloader:
                    raise ValueError(
                        "official vboot output does not preserve the padded bootloader"
                    )
        os.replace(temporary, output)

        recorded_pack = [
            str(output) if value == str(temporary) else value for value in command
        ]
        recorded_verify = [
            str(output) if value == str(temporary) else value for value in verify
        ]

    inputs = [image, config, keyblock, signprivate]
    inputs.append(signpubkey)
    if bootloader:
        inputs.append(bootloader)
    manifest = {
        "schema_version": 1,
        "tool": {"path": futility, "version": tool_version},
        "commands": {"pack": recorded_pack, "verify": recorded_verify},
        "parameters": {
            "architecture": "arm",
            "kernel_version": args.version,
            "verification_padding": args.pad,
            "kernel_body_load_address": kloadaddr,
        },
        "verify_output": verified.stdout,
        "inputs": {
            str(path): {"size": path.stat().st_size, "sha256": digest(path)}
            for path in inputs
        },
        "output": {
            "path": str(output),
            "size": output.stat().st_size,
            "sha256": digest(output),
            "structure": summary,
        },
    }
    manifest_path = output.with_suffix(output.suffix + ".manifest.json")
    manifest_path.write_text(
        json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    print(output)
    print(manifest_path)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
