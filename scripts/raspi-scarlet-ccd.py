#!/usr/bin/env python3
"""Pi-owned CCD USB consoles, accessed through a root-only Unix socket over SSH."""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import queue
import select
import socket
import socketserver
import sys
import termios
import threading
import time
import tty

TARGETS = {"cr50": 0, "ap": 1, "ec": 2}
KEYS = {
    "ctrl-l": b"kbpress 0 2 1\nkbpress 9 4 1\nkbpress 9 4 0\nkbpress 0 2 0\n",
    "ctrl-d": b"kbpress 0 2 1\nkbpress 2 4 1\nkbpress 2 4 0\nkbpress 0 2 0\n",
    "enter": b"kbpress 11 4 1\nkbpress 11 4 0\n",
    "release": b"kbpress clear\n",
}


class Hub:
    def __init__(self):
        self.lock = threading.Lock()
        self.operation = threading.Lock()
        self.stopping = threading.Event()
        self.connected = False
        self.error = "waiting for CCD"
        self.history = {target: bytearray() for target in TARGETS}
        self.listeners = {target: set() for target in TARGETS}
        self.writes = queue.Queue(maxsize=64)

    def publish(self, target, data):
        with self.lock:
            history = self.history[target]
            history.extend(data)
            del history[:-65536]
            for listener in tuple(self.listeners[target]):
                try:
                    listener.put_nowait(data)
                except queue.Full:
                    # Disconnect a slow reader instead of silently losing serial bytes.
                    self.listeners[target].remove(listener)
                    while True:
                        try:
                            listener.get_nowait()
                        except queue.Empty:
                            break
                    listener.put_nowait(None)

    def write(self, target, data):
        if not data or len(data) > 4096:
            raise ValueError("write must contain 1..4096 bytes")
        with self.lock:
            if not self.connected:
                raise RuntimeError(self.error)
            done = threading.Event()
            result = []
            self.writes.put_nowait((target, data, done, result))
        done.wait()  # USB worker completes or fails every queued request.
        if result:
            raise RuntimeError(result[0])

    def usb_loop(self, device_id, serial):
        import usb.core
        import usb.util
        vendor, product = (int(part, 16) for part in device_id.split(":"))
        reported_error = None
        while not self.stopping.is_set():
            device = None
            claimed = []
            detached = []
            try:
                devices = list(usb.core.find(find_all=True, idVendor=vendor, idProduct=product))
                if serial:
                    devices = [d for d in devices if usb.util.get_string(d, d.iSerialNumber) == serial]
                if len(devices) != 1:
                    raise RuntimeError(f"expected one CCD device, found {len(devices)}; set SCARLET_CCD_SERIAL if needed")
                device = devices[0]
                config = device.get_active_configuration()
                endpoints = {}
                for target, interface in TARGETS.items():
                    if device.is_kernel_driver_active(interface):
                        device.detach_kernel_driver(interface)
                        detached.append(interface)
                    usb.util.claim_interface(device, interface)
                    claimed.append(interface)
                    intf = usb.util.find_descriptor(config, bInterfaceNumber=interface)
                    if intf is None:
                        raise RuntimeError(f"missing CCD interface {interface}")
                    rx = usb.util.find_descriptor(intf, bEndpointAddress=0x80 | (interface + 1))
                    tx = usb.util.find_descriptor(intf, bEndpointAddress=interface + 1)
                    if rx is None or tx is None:
                        raise RuntimeError(f"missing CCD endpoints for {target}")
                    endpoints[target] = (rx, tx)
                def receive():
                    # Short polls keep a busy AP UART draining while EC/Cr50 are idle.
                    for target, (rx, _) in endpoints.items():
                        try:
                            data = bytes(rx.read(rx.wMaxPacketSize, timeout=1))
                            if data:
                                self.publish(target, data)
                        except usb.core.USBTimeoutError:
                            pass

                with self.lock:
                    self.connected, self.error = True, ""
                print("CCD connected", flush=True)
                reported_error = None
                while not self.stopping.is_set():
                    try:
                        target, data, done, result = self.writes.get_nowait()
                    except queue.Empty:
                        pass
                    else:
                        try:
                            # ChromiumOS console uses individual byte writes.
                            deadline = time.monotonic() + 5
                            for index, byte in enumerate(data):
                                if time.monotonic() >= deadline:
                                    raise RuntimeError("CCD write exceeded five seconds; command may be incomplete")
                                endpoints[target][1].write(bytes([byte]), timeout=1000)
                                if index % 16 == 15:
                                    receive()
                        except Exception as exc:
                            result.append(str(exc))
                            raise
                        finally:
                            done.set()
                    receive()
            except Exception as exc:
                with self.lock:
                    self.connected, self.error = False, str(exc)
                    while True:
                        try:
                            _, _, done, result = self.writes.get_nowait()
                        except queue.Empty:
                            break
                        result.append(str(exc))
                        done.set()
                if str(exc) != reported_error:
                    print(f"CCD unavailable: {exc}", file=sys.stderr, flush=True)
                    reported_error = str(exc)
            finally:
                if device is not None:
                    for interface in reversed(claimed):
                        try:
                            usb.util.release_interface(device, interface)
                        except usb.core.USBError:
                            pass
                    for interface in detached:
                        try:
                            device.attach_kernel_driver(interface)
                        except usb.core.USBError:
                            pass
                    usb.util.dispose_resources(device)
            self.stopping.wait(2)


def dispatch(hub, request):
    command = request.get("command")
    target = request.get("target", "ec")
    if not isinstance(target, str) or target not in TARGETS:
        raise ValueError("target must be cr50, ap, or ec")
    if command == "status":
        with hub.lock:
            return {"connected": hub.connected, "last_error": hub.error, "targets": list(TARGETS)}
    if command == "log":
        with hub.lock:
            return {"text": hub.history[target].decode("utf-8", errors="replace")}
    if command not in ("key", "reset", "boot", "send"):
        raise ValueError("unknown command")
    key = request.get("key", "ctrl-l")
    delay = request.get("delay_ms", 0)
    if not isinstance(key, str) or key not in KEYS or type(delay) is not int or not 0 <= delay <= 120000:
        raise ValueError("invalid key or delay (0..120000 ms)")
    payload = None
    if command == "send":
        payload = request.get("text")
        if not isinstance(payload, str) or not 1 <= len(payload.encode()) <= 4095:
            raise ValueError("text must contain 1..4095 UTF-8 bytes")
    # Serialize entire reset/delay/key sequences and console writes.
    if not hub.operation.acquire(blocking=False):
        raise RuntimeError("CCD control sequence is busy")
    def write_command(target, payload):
        # EC UART low-power wake can consume its first byte. Wake with a newline
        # and let it settle before submitting the actual console command.
        hub.write(target, b"\n")
        time.sleep(0.05)
        hub.write(target, payload)

    try:
        if command in ("reset", "boot"):
            write_command("ec", b"apreset\n")
        if command in ("key", "boot"):
            time.sleep(delay / 1000)
            write_command("ec", KEYS[key])
        if command == "send":
            write_command(target, payload.encode() + b"\n")
    finally:
        hub.operation.release()
    return {"ok": True}


class Handler(socketserver.StreamRequestHandler):
    def handle(self):
        hub = self.server.hub
        self.connection.settimeout(10)
        try:
            line = self.rfile.readline(8193)
            if len(line) > 8192 or not line.endswith(b"\n"):
                raise ValueError("invalid request size")
            request = json.loads(line)
            if not isinstance(request, dict):
                raise ValueError("request must be an object")
            if request.get("command") == "console":
                self.console(hub, request["target"])
                return
            response = dispatch(hub, request)
            self.wfile.write(json.dumps(response).encode() + b"\n")
        except (ValueError, RuntimeError, OSError, KeyError, queue.Full) as exc:
            try:
                self.wfile.write(json.dumps({"error": str(exc)}).encode() + b"\n")
            except OSError:
                pass

    def console(self, hub, target):
        if not isinstance(target, str) or target not in TARGETS:
            raise ValueError("target must be cr50, ap, or ec")
        listener = queue.Queue(maxsize=256)
        with hub.lock:
            history = bytes(hub.history[target])
            hub.listeners[target].add(listener)
        try:
            self.wfile.write(b'{"ok":true}\n')
            self.wfile.write(history)
            # No buffered reads after the protocol header: client waits for ACK.
            while True:
                readable, _, _ = select.select([self.connection], [], [], 0.05)
                if readable:
                    data = self.connection.recv(4096)
                    if not data:
                        return
                    if not hub.operation.acquire(blocking=False):
                        return  # Do not interleave bytes with a boot/key sequence.
                    try:
                        hub.write(target, data)
                    finally:
                        hub.operation.release()
                try:
                    data = listener.get_nowait()
                except queue.Empty:
                    continue
                if data is None:
                    return
                self.wfile.write(data)
        finally:
            with hub.lock:
                hub.listeners[target].discard(listener)


class Server(socketserver.ThreadingUnixStreamServer):
    daemon_threads = True


def serve(args):
    path = Path(args.socket)
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    # RuntimeDirectory is root-owned. systemd ensures only one daemon runs.
    if path.exists():
        with socket.socket(socket.AF_UNIX) as probe:
            try:
                probe.connect(str(path))
            except ConnectionRefusedError:
                path.unlink()
            else:
                raise RuntimeError("CCD service already running")
    os.umask(0o077)
    hub = Hub()
    # Import before starting: missing dependencies must fail systemd startup.
    import usb.core
    threading.Thread(target=hub.usb_loop, args=(args.device, args.serial), daemon=True).start()
    with Server(str(path), Handler) as server:
        server.hub = hub
        server.serve_forever()


def client(args):
    request = {"command": args.command, "target": args.target, "key": args.key,
               "delay_ms": args.delay_ms, "text": args.text}
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(140)
        connection.connect(args.socket)
        connection.sendall(json.dumps(request).encode() + b"\n")
        response = bytearray()
        while not response.endswith(b"\n"):
            data = connection.recv(1)
            if not data:
                raise RuntimeError("service disconnected before reply")
            response.extend(data)
        reply = json.loads(response)
        if reply.get("error"):
            raise RuntimeError(reply["error"])
        if args.command != "console":
            print(reply.get("text", json.dumps(reply)), end="" if args.command == "log" else "\n")
            return
        connection.settimeout(None)
        fd = sys.stdin.fileno()
        previous = termios.tcgetattr(fd) if os.isatty(fd) else None
        try:
            if previous is not None:
                tty.setraw(fd)
            inputs = [connection, fd]
            while True:
                readable, _, _ = select.select(inputs, [], [])
                if connection in readable:
                    data = connection.recv(4096)
                    if not data:
                        break
                    sys.stdout.buffer.write(data)
                    sys.stdout.buffer.flush()
                if fd in readable:
                    data = os.read(fd, 4096)
                    if not data:
                        break
                    connection.sendall(data)
        finally:
            if previous is not None:
                termios.tcsetattr(fd, termios.TCSADRAIN, previous)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["serve", "status", "reset", "key", "boot", "send", "console", "log"])
    parser.add_argument("--socket", default=os.environ.get("SCARLET_CCD_SOCKET", "/run/scarlet-ccd/control.sock"))
    parser.add_argument("--device", default=os.environ.get("SCARLET_CCD_DEVICE", "18d1:5014"))
    parser.add_argument("--serial", default=os.environ.get("SCARLET_CCD_SERIAL", ""))
    parser.add_argument("--target", choices=TARGETS, default="ec")
    parser.add_argument("--key", choices=KEYS, default="ctrl-l")
    parser.add_argument("--delay-ms", type=int, default=0)
    parser.add_argument("--text", default="")
    args = parser.parse_args()
    if not 0 <= args.delay_ms <= 120000:
        parser.error("delay must be 0..120000 ms")
    import re
    if not re.fullmatch(r"[0-9a-fA-F]{4}:[0-9a-fA-F]{4}", args.device):
        parser.error("device must be VID:PID")
    try:
        serve(args) if args.command == "serve" else client(args)
    except (OSError, RuntimeError, ValueError) as exc:
        print(f"scarlet-ccd: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
