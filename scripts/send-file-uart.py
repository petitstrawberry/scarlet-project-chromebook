#!/usr/bin/env python3
"""Send one file to a shell over the CoachZ AP UART PTY.

The remote shell must provide base64, gzip, sha256sum, stty, and wc.  Payload
bytes are gzip-compressed, base64-framed into short canonical-mode lines, and
rate-limited for the 115200-baud AP UART.  The remote size and SHA-256 must
match before the command succeeds.
"""

from __future__ import annotations

import argparse
import base64
import gzip
import hashlib
import os
from pathlib import Path
import secrets
import shlex
import sys
import time


def write_all(fd: int, data: bytes) -> None:
    view = memoryview(data)
    while view:
        written = os.write(fd, view)
        view = view[written:]


def wait_for_marker(
    log_path: Path, start: int, marker: bytes, timeout: float
) -> tuple[int, bytes]:
    deadline = time.monotonic() + timeout
    collected = bytearray()
    offset = start
    while time.monotonic() < deadline:
        with log_path.open("rb") as log:
            log.seek(offset)
            chunk = log.read()
        if chunk:
            collected.extend(chunk)
            offset += len(chunk)
            marker_offset = collected.find(marker)
            if marker_offset >= 0:
                remainder = collected[marker_offset + len(marker) :]
                if b"\n" in remainder or b"\r" in remainder:
                    return offset, bytes(collected)
        time.sleep(0.05)
    raise TimeoutError(f"timed out waiting for UART marker {marker!r}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("input", type=Path)
    parser.add_argument("--tty", type=Path, default=Path("/tmp/coachz-ap-uart"))
    parser.add_argument("--raw-log", type=Path, default=Path("work/uart/coachz-ap.raw"))
    parser.add_argument("--remote", default="/tmp/u-boot.elf")
    parser.add_argument("--rate", type=int, default=6000, help="encoded bytes/second")
    parser.add_argument("--ready-timeout", type=float, default=10.0)
    parser.add_argument("--done-timeout", type=float, default=30.0)
    args = parser.parse_args()

    if args.rate <= 0:
        parser.error("--rate must be positive")
    if not args.input.is_file():
        parser.error(f"input does not exist: {args.input}")
    if not args.tty.exists():
        parser.error(f"UART PTY does not exist: {args.tty}")
    if not args.raw_log.is_file():
        parser.error(f"raw UART log does not exist: {args.raw_log}")

    source = args.input.read_bytes()
    source_hash = hashlib.sha256(source).hexdigest()
    encoded = base64.encodebytes(gzip.compress(source, compresslevel=9, mtime=0))
    token = secrets.token_hex(8)
    ready = f"SCARLET_UART_READY_{token}".encode()
    done = f"SCARLET_UART_DONE_{token}".encode()
    remote = shlex.quote(args.remote)

    receiver = (
        "set -o pipefail; "
        f"printf '\\n{ready.decode()}\\n'; "
        f"base64 -d | gzip -d > {remote}; rc=$?; "
        "stty echo; "
        f"size=$(wc -c < {remote}); "
        f"set -- $(sha256sum {remote}); sha=$1; "
        f"printf '\\n{done.decode()} rc=%s size=%s sha256=%s\\n' "
        '"$rc" "$size" "$sha"'
    )

    log_offset = args.raw_log.stat().st_size
    fd = os.open(args.tty, os.O_WRONLY | os.O_NOCTTY)
    try:
        write_all(fd, b"stty -echo\r")
        time.sleep(0.4)
        write_all(fd, receiver.encode() + b"\r")
        log_offset, _ = wait_for_marker(
            args.raw_log, log_offset, ready, args.ready_timeout
        )

        print(
            f"sending {len(source)} bytes as {len(encoded)} encoded bytes "
            f"at {args.rate} B/s",
            flush=True,
        )
        started = time.monotonic()
        sent = 0
        for offset in range(0, len(encoded), 512):
            chunk = encoded[offset : offset + 512]
            write_all(fd, chunk)
            sent += len(chunk)
            target_elapsed = sent / args.rate
            delay = target_elapsed - (time.monotonic() - started)
            if delay > 0:
                time.sleep(delay)
        write_all(fd, b"\x04")

        _, output = wait_for_marker(
            args.raw_log, log_offset, done, args.done_timeout
        )
    except Exception:
        write_all(fd, b"\x03\rstty echo\r")
        raise
    finally:
        os.close(fd)

    marker_start = output.rfind(done)
    result_line = output[marker_start:].splitlines()[0].decode(errors="replace")
    expected = f"{done.decode()} rc=0 size={len(source)} sha256={source_hash}"
    print(result_line)
    if result_line != expected:
        print(f"error: expected {expected}", file=sys.stderr)
        return 1
    print(f"verified remote file {args.remote}: SHA-256 {source_hash}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
