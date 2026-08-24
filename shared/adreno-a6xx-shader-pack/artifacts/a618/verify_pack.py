#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only
"""Verify an A618 shader pack with a pinned, independently built Mesa decoder."""
import argparse
import hashlib
import json
import pathlib
import re
import subprocess
import sys

EXPECTED_SHA = "3f1b217baffffa00cb8f53e158713a33e1bd4632"

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument(
    "--pack",
    type=pathlib.Path,
    default=pathlib.Path(__file__).resolve().parent,
    help="directory containing the generated pack (default: this script's directory)",
)
parser.add_argument("--mesa", type=pathlib.Path, required=True,
                    help="Mesa checkout at the pinned commit")
parser.add_argument("--ir3-disasm", type=pathlib.Path, required=True,
                    help="independently built ir3-disasm binary")
parser.add_argument(
    "--expected-pack",
    type=pathlib.Path,
    help="optional shipped pack to compare byte-for-byte after validation",
)
args = parser.parse_args()
PACK = args.pack.resolve()
MESA = args.mesa.resolve()
DISASM = args.ir3_disasm.resolve()

if not DISASM.is_file():
    raise SystemExit(f"ir3-disasm not found: {DISASM}")
head = subprocess.check_output(["git", "-C", str(MESA), "rev-parse", "HEAD"], text=True).strip()
assert head == EXPECTED_SHA, (head, EXPECTED_SHA)
metadata = json.loads((PACK / "mesa-metadata.json").read_text())
assert metadata["mesa_sha"] == EXPECTED_SHA
assert metadata["gpu_id"] == 618
variants = metadata["variants"]
assert len(variants) == 13
assert len({variant["name"] for variant in variants}) == len(variants)

manifest = []
for variant in variants:
    name = variant["name"]
    binary = PACK / f"{name}.bin"
    expected_disasm = (PACK / f"{name}.disasm").read_text().splitlines()
    payload = binary.read_bytes()
    assert len(payload) == variant["binary_bytes"] == variant["sizedwords"] * 4
    assert len(payload) % 128 == 0
    decoded = subprocess.run(
        [str(DISASM), "-g", "FD618", str(binary)],
        check=True,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    assert not decoded.stderr
    assert "error" not in decoded.stdout.lower()
    decoded_instructions = []
    for line in decoded.stdout.splitlines():
        match = re.match(r"\s*\d+\[[0-9a-f]+_[0-9a-f]+\]\s+(.*)", line)
        assert match, line
        decoded_instructions.append(match.group(1))
    assert decoded_instructions[: len(expected_disasm)] == expected_disasm, name
    manifest.append((name, len(payload), hashlib.sha256(payload).hexdigest()))

links = metadata["links"]
names = {variant["name"] for variant in variants}
assert len(links) == 10
for link in links:
    assert link["vs"] in names and link["fs"] in names

if args.expected_pack:
    expected = args.expected_pack.resolve()
    stable_names = [
        "mesa-metadata.json",
        "packed-state.json",
        *[f"{variant['name']}{suffix}" for variant in variants for suffix in (".bin", ".disasm")],
    ]
    for name in stable_names:
        actual = PACK / name
        reference = expected / name
        assert actual.read_bytes() == reference.read_bytes(), name
    print(f"byte_for_byte_matches_expected_pack={expected}")

print(f"mesa_sha={head}")
print(f"gpu_id={metadata['gpu_id']}")
print(f"variants={len(variants)} links={len(links)}")
for name, size, digest in manifest:
    print(f"{digest}  {size:3d}  {name}.bin")
print("all_metadata_binary_and_independent_disassembly_checks=PASS")
