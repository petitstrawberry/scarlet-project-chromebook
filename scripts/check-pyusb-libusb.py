#!/usr/bin/env python3
"""Verify PyUSB's libusb1 backend, optionally listing matching USB devices."""

from __future__ import annotations

import argparse
import sys


DEFAULT_VID_PID = "18d1:5014"


def parse_vid_pid(value: str) -> tuple[int, int]:
    try:
        vendor, product = value.split(":", 1)
        if len(vendor) != 4 or len(product) != 4:
            raise ValueError
        return int(vendor, 16), int(product, 16)
    except ValueError as exc:
        raise argparse.ArgumentTypeError(
            f"expected VID:PID as four hexadecimal digits each, got {value!r}"
        ) from exc


def main() -> int:
    parser = argparse.ArgumentParser(
        description=(
            "Check that PyUSB can import and load its libusb1 backend. "
            "No USB device is accessed unless --list is supplied."
        )
    )
    parser.add_argument(
        "--list",
        action="store_true",
        help="list devices matching --vidpid after checking the backend",
    )
    parser.add_argument(
        "--vidpid",
        default=DEFAULT_VID_PID,
        type=parse_vid_pid,
        metavar="VID:PID",
        help=f"device to list (default: {DEFAULT_VID_PID})",
    )
    args = parser.parse_args()

    try:
        import usb.backend.libusb1
        import usb.core
    except ImportError as exc:
        print(f"FAIL: PyUSB import failed: {exc}", file=sys.stderr)
        return 1

    try:
        backend = usb.backend.libusb1.get_backend()
    except Exception as exc:  # Include dynamic-loader failures with useful context.
        print(f"FAIL: libusb1 backend initialization raised {exc!r}", file=sys.stderr)
        return 1

    if backend is None:
        print(
            "FAIL: PyUSB imported, but its libusb1 backend could not be loaded.",
            file=sys.stderr,
        )
        return 1

    print(f"PASS: PyUSB libusb1 backend loaded ({type(backend).__name__})")
    if not args.list:
        return 0

    vendor, product = args.vidpid
    try:
        devices = list(
            usb.core.find(
                find_all=True, idVendor=vendor, idProduct=product, backend=backend
            )
        )
    except usb.core.USBError as exc:
        print(f"FAIL: unable to list {vendor:04x}:{product:04x}: {exc}", file=sys.stderr)
        return 1

    print(f"Matching devices for {vendor:04x}:{product:04x}: {len(devices)}")
    for device in devices:
        bus = "?" if device.bus is None else str(device.bus)
        address = "?" if device.address is None else str(device.address)
        print(f"  bus={bus} address={address}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
