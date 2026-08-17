#!/usr/bin/env python3
"""Receive one file over a short-lived local HTTP PUT.

This is intentionally a small, one-shot bridge for bringing artifacts back from
a Chromebook whose only currently available control channel is AP UART.  Bind
it to the host's private LAN address and remove the temporary listener after the
transfer; it is not a general-purpose file server.
"""

from __future__ import annotations

import argparse
import hashlib
import http.server
import os
from pathlib import Path
import socketserver
import sys


class _Server(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bind", required=True, help="private host address")
    parser.add_argument("--port", type=int, default=8765)
    parser.add_argument("--path", default="/upload")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--size", type=int, required=True)
    parser.add_argument("--sha256", required=True)
    args = parser.parse_args()

    if args.size < 0 or len(args.sha256) != 64:
        parser.error("--size must be non-negative and --sha256 must be 64 hex digits")
    expected_sha = args.sha256.lower()
    if any(c not in "0123456789abcdef" for c in expected_sha):
        parser.error("--sha256 must be hexadecimal")

    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, fmt: str, *values: object) -> None:
            print(fmt % values, file=sys.stderr, flush=True)

        def do_PUT(self) -> None:  # noqa: N802 - stdlib callback name
            if self.path != args.path:
                self.send_error(http.HTTPStatus.NOT_FOUND)
                return
            raw_length = self.headers.get("Content-Length")
            try:
                length = int(raw_length) if raw_length is not None else -1
            except ValueError:
                length = -1
            if length != args.size:
                self.send_error(
                    http.HTTPStatus.LENGTH_REQUIRED,
                    f"expected Content-Length {args.size}, got {raw_length!r}",
                )
                return

            args.output.parent.mkdir(parents=True, exist_ok=True)
            part = args.output.with_name(args.output.name + ".part")
            digest = hashlib.sha256()
            received = 0
            try:
                with part.open("wb") as target:
                    while received < args.size:
                        chunk = self.rfile.read(min(1024 * 1024, args.size - received))
                        if not chunk:
                            raise OSError("client closed before Content-Length bytes arrived")
                        target.write(chunk)
                        digest.update(chunk)
                        received += len(chunk)
                actual = digest.hexdigest()
                if actual != expected_sha:
                    part.unlink(missing_ok=True)
                    self.send_error(
                        http.HTTPStatus.UNPROCESSABLE_ENTITY,
                        f"SHA-256 mismatch: expected {expected_sha}, got {actual}",
                    )
                    return
                os.replace(part, args.output)
            except Exception as exc:  # Keep the one-shot server failure explicit.
                part.unlink(missing_ok=True)
                self.send_error(http.HTTPStatus.INTERNAL_SERVER_ERROR, str(exc))
                return

            self.send_response(http.HTTPStatus.CREATED)
            self.send_header("Content-Type", "text/plain; charset=utf-8")
            self.end_headers()
            self.wfile.write(f"received {received} bytes SHA-256 {actual}\n".encode())
            print(f"received {args.output} ({received} bytes), SHA-256 {actual}", flush=True)
            self.server.shutdown()

        def do_POST(self) -> None:  # noqa: N802 - accept curl -X POST too
            self.do_PUT()

    with _Server((args.bind, args.port), Handler) as server:
        print(f"listening on http://{args.bind}:{args.port}{args.path}", flush=True)
        server.serve_forever()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
