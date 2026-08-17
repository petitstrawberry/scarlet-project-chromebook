#!/usr/bin/env python3
"""Expose a ChromiumOS EC USB console as a local pseudo-terminal."""

from __future__ import annotations

import argparse
import fcntl
import os
from pathlib import Path
import pty
import select
import signal
import subprocess
import sys
import termios
import tty


TARGETS = {"cr50": 0, "ap": 1, "ec": 2}


class CrLfNormalizer:
    """Convert bare LF to CRLF without changing existing CRLF sequences."""

    def __init__(self) -> None:
        self._previous_was_cr = False

    def feed(self, data: bytes) -> bytes:
        output = bytearray()
        for byte in data:
            if byte == 0x0A and not self._previous_was_cr:
                output.append(0x0D)
            output.append(byte)
            self._previous_was_cr = byte == 0x0D
        return bytes(output)


def replace_link(link: Path, slave_name: str) -> None:
    if link.is_symlink():
        link.unlink()
    elif link.exists():
        raise RuntimeError(f"refusing to replace non-symlink path: {link}")
    link.symlink_to(slave_name)


def main() -> int:
    parser = argparse.ArgumentParser(
        description=(
            "Bridge a CCD USB console to a PTY for minicom, picocom, or screen."
        )
    )
    parser.add_argument("target", choices=TARGETS)
    parser.add_argument("--device", default="18d1:5014", metavar="VID:PID")
    parser.add_argument("--serial", default="")
    parser.add_argument("--link", type=Path)
    parser.add_argument(
        "--raw-log",
        type=Path,
        help="append exact USB receive bytes to this file before display conversion",
    )
    parser.add_argument(
        "--no-crlf",
        action="store_true",
        help="do not convert bare LF to CRLF on the PTY display stream",
    )
    args = parser.parse_args()

    script_dir = Path(__file__).resolve().parent
    launcher = script_dir / "ec-usb-console.sh"
    link = args.link or Path(f"/tmp/coachz-{args.target}-uart")
    lock_path = Path(f"{link}.lock")
    lock_path.parent.mkdir(parents=True, exist_ok=True)
    lock_file = lock_path.open("a+")
    try:
        fcntl.flock(lock_file, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        print(f"error: another bridge holds {lock_path}", file=sys.stderr)
        return 2

    master_fd, slave_fd = pty.openpty()
    tty.setraw(slave_fd)
    attrs = termios.tcgetattr(slave_fd)
    attrs[3] &= ~termios.ECHO
    termios.tcsetattr(slave_fd, termios.TCSANOW, attrs)
    slave_name = os.ttyname(slave_fd)

    command = [
        str(launcher),
        "--forward-ctrl-c",
        "--target",
        args.target,
        "--device",
        args.device,
    ]
    if args.serial:
        command.extend(["--serial", args.serial])

    raw_log = None
    child: subprocess.Popen[bytes] | None = None
    stopping = False

    def stop(_signum: int, _frame: object) -> None:
        nonlocal stopping
        stopping = True

    signal.signal(signal.SIGINT, stop)
    signal.signal(signal.SIGTERM, stop)

    try:
        replace_link(link, slave_name)
        if args.raw_log:
            args.raw_log.parent.mkdir(parents=True, exist_ok=True)
            raw_log = args.raw_log.open("ab", buffering=0)

        child = subprocess.Popen(
            command,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=None,
            bufsize=0,
        )
        assert child.stdin is not None
        assert child.stdout is not None
        child_stdin = child.stdin.fileno()
        child_stdout = child.stdout.fileno()
        normalizer = CrLfNormalizer()

        print(f"PTY: {link} -> {slave_name}", file=sys.stderr)
        if args.raw_log:
            print(f"raw RX log: {args.raw_log}", file=sys.stderr)
        print(
            f"attach with: minicom -o -D {link} -b 115200",
            file=sys.stderr,
        )

        while not stopping and child.poll() is None:
            readable, _, _ = select.select(
                [master_fd, child_stdout], [], [], 0.1
            )
            if child_stdout in readable:
                data = os.read(child_stdout, 4096)
                if not data:
                    break
                if raw_log:
                    raw_log.write(data)
                display = data if args.no_crlf else normalizer.feed(data)
                os.write(master_fd, display)

            if master_fd in readable:
                try:
                    data = os.read(master_fd, 4096)
                except OSError:
                    continue
                if data:
                    os.write(child_stdin, data)

        return_code = child.poll()
        if return_code not in (None, 0) and not stopping:
            print(f"error: USB console exited with status {return_code}", file=sys.stderr)
            return return_code
        return 0
    finally:
        if child and child.poll() is None:
            child.terminate()
            try:
                child.wait(timeout=2)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait()
        if raw_log:
            raw_log.close()
        os.close(master_fd)
        os.close(slave_fd)
        if link.is_symlink() and os.readlink(link) == slave_name:
            link.unlink()
        lock_file.close()


if __name__ == "__main__":
    raise SystemExit(main())
