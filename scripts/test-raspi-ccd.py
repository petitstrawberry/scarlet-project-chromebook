#!/usr/bin/env python3
"""Hardware-independent service/protocol/SSH regression tests."""
import importlib.util
import json
import os
from pathlib import Path
import queue
import shlex
import socket
import subprocess
import tempfile
import threading
import time
import types
import unittest
from unittest.mock import patch

SCRIPTS = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location('ccd', SCRIPTS / 'raspi-scarlet-ccd.py')
ccd = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ccd)


class HubTests(unittest.TestCase):
    def setUp(self):
        self.hub = ccd.Hub()
        self.sent = []
        self.hub.write = lambda target, data: self.sent.append((target, data))

    def test_boot_order(self):
        with patch.object(ccd.time, 'sleep') as sleep:
            ccd.dispatch(self.hub, {'command': 'boot', 'delay_ms': 123, 'key': 'ctrl-l'})
        self.assertEqual(self.sent, [('ec', b'\n'), ('ec', b'apreset\n'), ('ec', b'\n'), ('ec', ccd.KEYS['ctrl-l'])])
        self.assertEqual([call.args[0] for call in sleep.call_args_list], [.05, .123, .05])

    def test_no_reset_for_key(self):
        ccd.dispatch(self.hub, {'command': 'key', 'key': 'enter'})
        self.assertEqual(self.sent, [('ec', b'\n'), ('ec', ccd.KEYS['enter'])])

    def test_validation_before_hardware(self):
        for request in [
            {'command': 'boot', 'key': 'invalid'},
            {'command': 'boot', 'delay_ms': -1},
            {'command': 'boot', 'delay_ms': True},
            {'command': 'boot', 'delay_ms': 120001},
            {'command': 'boot', 'target': 'invalid'},
            {'command': 'send', 'text': 'x' * 4096},
            {'command': 'arbitrary-shell'},
        ]:
            with self.subTest(request=str(request)[:80]), self.assertRaises(ValueError):
                ccd.dispatch(self.hub, request)
        self.assertEqual(self.sent, [])

    def test_operation_exclusion_and_recovery(self):
        with self.hub.operation:
            with self.assertRaisesRegex(RuntimeError, 'busy'):
                ccd.dispatch(self.hub, {'command': 'reset'})
        ccd.dispatch(self.hub, {'command': 'reset'})
        self.assertEqual(self.sent, [('ec', b'\n'), ('ec', b'apreset\n')])

    def test_literal_console_command(self):
        ccd.dispatch(self.hub, {'command': 'send', 'target': 'ap', 'text': 'echo "x"; $(uname)'})
        self.assertEqual(self.sent, [('ap', b'\n'), ('ap', b'echo "x"; $(uname)\n')])

    def test_ring_and_slow_consumer(self):
        listener = queue.Queue(maxsize=1)
        self.hub.listeners['ap'].add(listener)
        self.hub.publish('ap', b'x' * 70000)
        self.hub.publish('ap', b'end')
        self.assertEqual(len(self.hub.history['ap']), 65536)
        self.assertTrue(self.hub.history['ap'].endswith(b'end'))
        self.assertIsNone(listener.get_nowait())
        self.assertNotIn(listener, self.hub.listeners['ap'])

    def test_disconnected_does_not_queue_command(self):
        hub = ccd.Hub()
        with self.assertRaisesRegex(RuntimeError, 'waiting'):
            hub.write('ec', b'apreset\n')
        self.assertTrue(hub.writes.empty())


class ProtocolTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix='ccd-', dir='/tmp')
        self.path = str(Path(self.tmp.name) / 'socket')
        self.server = ccd.Server(self.path, ccd.Handler)
        self.server.hub = ccd.Hub()
        self.sent = []
        self.server.hub.write = lambda target, data: self.sent.append((target, data))
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()
        self.tmp.cleanup()

    def connect(self):
        connection = socket.socket(socket.AF_UNIX)
        connection.settimeout(2)
        connection.connect(self.path)
        self.addCleanup(connection.close)
        return connection

    def test_request_error_does_not_stop_service(self):
        for request in [b'[]\n', b'x\n', b'x'*8193]:
            connection = self.connect()
            connection.sendall(request)
            self.assertIn('error', json.loads(connection.makefile('rb').readline()))
        result = subprocess.run(['python3', str(SCRIPTS/'raspi-scarlet-ccd.py'), 'status', '--socket', self.path], capture_output=True, text=True, timeout=3)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(json.loads(result.stdout)['connected'])

    def test_console_fanout_and_control_while_attached(self):
        readers = []
        self.server.hub.publish('ap', b'before\n')
        for _ in range(2):
            connection = self.connect()
            connection.sendall(b'{"command":"console","target":"ap"}\n')
            reader = connection.makefile('rb')
            self.addCleanup(reader.close)
            self.assertTrue(json.loads(reader.readline())['ok'])
            self.assertEqual(reader.readline(), b'before\n')
            readers.append(reader)
        control = self.connect()
        control.sendall(b'{"command":"reset"}\n')
        self.assertTrue(json.loads(control.makefile('rb').readline())['ok'])
        self.assertEqual(self.sent, [('ec', b'\n'), ('ec', b'apreset\n')])
        self.server.hub.publish('ap', b'after\n')
        for reader in readers:
            self.assertEqual(reader.readline(), b'after\n')


class USBWorkerTests(unittest.TestCase):
    def test_claim_read_write_disconnect_and_reconnect(self):
        hub = ccd.Hub()
        class USBError(Exception):
            pass
        class USBTimeoutError(USBError):
            pass
        writes, claims, releases, enumerations = [], [], [], []
        class Endpoint:
            wMaxPacketSize = 64
            def read(self, count, timeout):
                time.sleep(.001)
                raise USBTimeoutError()
            def write(self, data, timeout):
                if data == b'!':
                    raise USBError('unplugged')
                writes.append(data)
        class Device:
            def get_active_configuration(self): return object()
            def is_kernel_driver_active(self, interface): return False
        device = Device()
        def find(**kwargs):
            enumerations.append(True)
            return [device]
        core = types.ModuleType('usb.core')
        core.find, core.USBError, core.USBTimeoutError = find, USBError, USBTimeoutError
        util = types.ModuleType('usb.util')
        util.find_descriptor = lambda *args, **kwargs: Endpoint()
        util.claim_interface = lambda d, i: claims.append(i)
        util.release_interface = lambda d, i: releases.append(i)
        util.dispose_resources = lambda d: None
        usb = types.ModuleType('usb')
        usb.core, usb.util = core, util
        def wait_for(predicate):
            deadline = time.monotonic() + 5
            while not predicate():
                if time.monotonic() > deadline:
                    self.fail('USB worker did not reach expected state')
                time.sleep(.01)
        with patch.dict('sys.modules', {'usb': usb, 'usb.core': core, 'usb.util': util}):
            thread = threading.Thread(target=hub.usb_loop, args=('18d1:5014', ''), daemon=True)
            thread.start()
            try:
                wait_for(lambda: hub.connected)
                hub.write('ec', b'hello')
                self.assertEqual(b''.join(writes), b'hello')
                with self.assertRaisesRegex(RuntimeError, 'unplugged'):
                    hub.write('ec', b'!')
                wait_for(lambda: len(enumerations) >= 2 and hub.connected)
                hub.write('ap', b'ok')
                self.assertEqual(b''.join(writes), b'hellook')
            finally:
                hub.stopping.set()
                thread.join(timeout=3)
            self.assertFalse(thread.is_alive())
            self.assertEqual(claims, [0, 1, 2, 0, 1, 2])
            self.assertEqual(releases, [2, 1, 0, 2, 1, 0])


class LauncherTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        ssh = self.root/'ssh'
        ssh.write_text('#!/usr/bin/env python3\nimport json,sys\nprint(json.dumps(sys.argv[1:]))\n')
        ssh.chmod(0o755)
        self.config = self.root/'config'
        self.config.write_text('SCARLET_RPI_HOST=pi-on-lan.local\nSCARLET_RPI_USER=tester\n')
        self.env = dict(os.environ, PATH=f'{self.root}:'+os.environ['PATH'], SCARLET_RPI_CLIENT_CONFIG=str(self.config))
        for key in ['SCARLET_RPI_HOST', 'SCARLET_RPI_USER', 'SCARLET_RPI_IDENTITY', 'SCARLET_CCD_BACKEND']:
            self.env.pop(key, None)

    def run_script(self, name, *args, **env):
        return subprocess.run(['bash', str(SCRIPTS/name), *args], env=dict(self.env, **env), capture_output=True, text=True)

    def run_local_boot(self, *args):
        boot = self.root / 'raspi-scarlet-boot.sh'
        boot.write_text((SCRIPTS / boot.name).read_text())
        ec = self.root / 'ec-usb-command.py'
        ec.write_text('#!/usr/bin/env python3\nimport sys\nsys.stdout.buffer.write(sys.stdin.buffer.read())\n')
        ec.chmod(0o755)
        return subprocess.run(
            ['bash', str(boot), '--backend', 'local', *args],
            env=self.env, capture_output=True, text=True,
        )

    def test_local_boot_accepts_decimal_delay(self):
        result = self.run_local_boot('--no-reset', '--delay-ms', '008', '--key', 'enter')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, ccd.KEYS['enter'].decode())

    def test_invalid_local_delay_never_resets_ap(self):
        for delay in ['-1', '120001', '9999999', 'invalid']:
            with self.subTest(delay=delay):
                result = self.run_local_boot('--delay-ms', delay)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn('delay must be', result.stderr)
                self.assertEqual(result.stdout, '')

    def test_boot_uses_remote_service(self):
        result = self.run_script('raspi-scarlet-boot.sh', '--delay-ms', '008')
        self.assertEqual(result.returncode, 0, result.stderr)
        args = json.loads(result.stdout)
        self.assertEqual(args[-2], 'tester@pi-on-lan.local')
        self.assertEqual(shlex.split(args[-1]), ['sudo', '-n', '/usr/local/sbin/scarlet-ccd', 'boot', '--delay-ms', '8', '--key', 'ctrl-l'])

    def test_environment_overrides_saved_host(self):
        result = self.run_script('raspi-scarlet-control.sh', 'status', SCARLET_RPI_HOST='other.local')
        self.assertEqual(json.loads(result.stdout)[-2], 'tester@other.local')

    def test_console_text_survives_remote_shell(self):
        literal = 'echo "hello"; $(touch /not-to-be-executed) `uname`'
        result = self.run_script('raspi-scarlet-control.sh', 'send', 'ap', literal)
        args = json.loads(result.stdout)
        self.assertEqual(shlex.split(args[-1])[-1], literal)

    def test_invalid_requests_never_connect(self):
        for args in [('boot','-1'), ('boot','120001'), ('console','invalid'), ('key','invalid')]:
            result = self.run_script('raspi-scarlet-control.sh', *args)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(result.stdout, '')


if __name__ == '__main__':
    unittest.main()
