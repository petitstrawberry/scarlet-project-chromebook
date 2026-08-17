#!/usr/bin/env python3
"""Run ChromiumOS EC console.py while forwarding Ctrl-C to the device."""

from __future__ import annotations

import importlib.util
from pathlib import Path
import sys


def main() -> int:
    if len(sys.argv) < 2:
        print(f"usage: {Path(sys.argv[0]).name} CONSOLE.PY [ARGS...]", file=sys.stderr)
        return 2

    console_path = Path(sys.argv[1]).resolve()
    spec = importlib.util.spec_from_file_location(
        "chromiumos_ec_usb_console", console_path
    )
    if spec is None or spec.loader is None:
        print(f"error: cannot load {console_path}", file=sys.stderr)
        return 2

    console = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(console)

    def run_tx_thread_forwarding_ctrl_c(self: object) -> None:
        """Forward every input byte; only EOF terminates the TX thread."""
        try:
            while True:
                try:
                    data = sys.stdin.buffer.read(1)
                    if not data:
                        break
                    self._susb._write_ep.write(
                        console.array.array("B", data), self._susb.TIMEOUT_MS
                    )
                except Exception as err:  # pylint: disable=broad-except
                    print(f"tx {err}")
        finally:
            self._done.set()

    console.Suart.run_tx_thread = run_tx_thread_forwarding_ctrl_c
    sys.argv = [str(console_path), *sys.argv[2:]]
    console.main()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
