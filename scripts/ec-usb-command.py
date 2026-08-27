#!/usr/bin/env python3
"""Send a finite command stream to a ChromiumOS EC USB console.

The upstream console.py keeps a receiver thread alive while stdin is consumed.
That is useful for an interactive terminal, but an AP reset can tear down the
USB device while the receiver is still active and make libusb crash on macOS.
This helper only performs the OUT transfers needed by one-shot commands.
"""

from __future__ import annotations

import argparse
import array
import sys
import time


DEFAULT_DEVICE = "18d1:5014"
DEFAULT_INTERFACE = 2


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


def find_device(usb, vendor: int, product: int, serial: str):
    devices = list(
        usb.core.find(
            find_all=True,
            idVendor=vendor,
            idProduct=product,
        )
    )
    if not devices:
        raise RuntimeError(f"USB device {vendor:04x}:{product:04x} was not found")
    if not serial:
        return devices[0]

    for device in devices:
        try:
            if usb.util.get_string(device, device.iSerialNumber) == serial:
                return device
        except usb.core.USBError:
            continue
    raise RuntimeError(f"USB device {serial!r} was not found")


def open_write_endpoint(usb, vendor: int, product: int, interface: int, serial: str):
    device = find_device(usb, vendor, product, serial)
    try:
        device.set_configuration()
    except usb.core.USBError:
        # A configuration that is already active is fine.
        pass

    configuration = device.get_active_configuration()
    intf = usb.util.find_descriptor(
        configuration,
        bInterfaceNumber=interface,
    )
    if intf is None:
        raise RuntimeError(f"USB interface {interface} was not found")

    detached = False
    try:
        if device.is_kernel_driver_active(interface):
            device.detach_kernel_driver(interface)
            detached = True
    except NotImplementedError:
        pass
    except usb.core.USBError as exc:
        raise RuntimeError(
            f"USB interface {interface} is busy; close the interactive EC console: {exc}"
        ) from exc

    endpoint = usb.util.find_descriptor(
        intf,
        bEndpointAddress=interface + 1,
    )
    if endpoint is None:
        if detached:
            try:
                device.attach_kernel_driver(interface)
            except usb.core.USBError:
                pass
        raise RuntimeError(
            f"USB OUT endpoint 0x{interface + 1:02x} was not found on interface {interface}"
        )
    return device, endpoint, detached


def release_interface(usb, device, interface: int, detached: bool) -> None:
    try:
        usb.util.release_interface(device, interface)
    except usb.core.USBError:
        pass
    if detached:
        try:
            device.attach_kernel_driver(interface)
        except (NotImplementedError, usb.core.USBError):
            pass


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Send stdin bytes to one ChromiumOS EC USB serial interface."
    )
    parser.add_argument(
        "-d",
        "--device",
        default=DEFAULT_DEVICE,
        type=parse_vid_pid,
        metavar="VID:PID",
    )
    parser.add_argument(
        "-i",
        "--interface",
        default=DEFAULT_INTERFACE,
        type=int,
        help="USB interface number (EC CCD console is 2)",
    )
    parser.add_argument(
        "-s",
        "--serial",
        default="",
        help="USB serial number when more than one matching device is present",
    )
    parser.add_argument(
        "--timeout-ms",
        default=1000,
        type=int,
        help="per-byte USB write timeout",
    )
    parser.add_argument(
        "--post-write-delay-ms",
        default=50,
        type=int,
        help="delay before releasing the USB interface",
    )
    args = parser.parse_args()

    if args.interface < 0:
        parser.error("--interface must be non-negative")
    if args.timeout_ms <= 0:
        parser.error("--timeout-ms must be positive")
    if args.post_write_delay_ms < 0:
        parser.error("--post-write-delay-ms must be non-negative")

    payload = sys.stdin.buffer.read()
    if not payload:
        return 0

    try:
        import usb.core
        import usb.util
    except ImportError as exc:
        print(f"error: PyUSB import failed: {exc}", file=sys.stderr)
        return 1

    vendor, product = args.device
    device = None
    detached = False
    try:
        device, endpoint, detached = open_write_endpoint(
            usb,
            vendor,
            product,
            args.interface,
            args.serial,
        )
        for byte in payload:
            endpoint.write(array.array("B", [byte]), args.timeout_ms)
        if args.post_write_delay_ms:
            time.sleep(args.post_write_delay_ms / 1000)
    except (RuntimeError, OSError, usb.core.USBError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1
    finally:
        if device is not None:
            release_interface(usb, device, args.interface, detached)

    print(
        f"sent {len(payload)} byte(s) to {vendor:04x}:{product:04x}"
        f" interface {args.interface}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
