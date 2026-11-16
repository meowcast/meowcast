#!/usr/bin/env python3
"""Run with the pairing venv: python -m unittest discover -s tools.

--interop is an IPC fixture used by the Rust test; all keys here are synthetic.
"""
import base64
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

import meowdbg
from spake2 import SPAKE2_B

KEY = 'ssh-ed25519 ' + base64.b64encode(b'\0\0\0\x0bssh-ed25519\0\0\0\x20' + bytes([7]) * 32).decode() + ' test'
HOST = 'ssh-ed25519 ' + base64.b64encode(b'\0\0\0\x0bssh-ed25519\0\0\0\x20' + bytes([8]) * 32).decode() + ' dongle'
DEVICE = 'a' * 32


class Server:
    def __init__(self, code='123456', tamper=False):
        self.code, self.tamper = code, tamper
        self.secret = self.payload = None
        self.finished = False

    def post(self, step, body):
        if step == 'start':
            state = SPAKE2_B(self.code.encode(), idA=b'meowdbg-v2/client', idB=b'meowdbg-v2/dongle')
            message = state.start()
            self.secret = state.finish(bytes.fromhex(body['message']))
            self.payload = json.dumps({'device': DEVICE, 'host_key': HOST, 'key': body['key'], 'session': 'test-session', 'version': 2}, separators=(',', ':'), sort_keys=True)
            proof = meowdbg.pairing_proof(self.secret, 'server', self.payload)
            payload = self.payload.replace('dongle', 'impostor') if self.tamper else self.payload
            return {'ok': True, 'session': 'test-session', 'message': message.hex(), 'payload': payload, 'proof': proof}
        assert step == 'finish'
        assert not self.finished
        assert body['proof'] == meowdbg.pairing_proof(self.secret, 'client', self.payload)
        self.finished = True
        return {'ok': True, 'proof': meowdbg.pairing_proof(self.secret, 'complete', self.payload)}


class PairingTests(unittest.TestCase):
    def test_success_pins_host_and_derives_untransmitted_credential(self):
        server = Server()
        result = meowdbg.pairing_exchange(KEY, '123456', server.post)
        self.assertTrue(server.finished)
        self.assertEqual(result['host_key'], meowdbg.validate_ssh_key(HOST))
        self.assertEqual(result['device'], DEVICE)
        self.assertEqual(result['token'], meowdbg.pairing_proof(server.secret, 'admission', server.payload))

    def test_wrong_code_never_enrolls(self):
        server = Server('654321')
        with self.assertRaises(ValueError):
            meowdbg.pairing_exchange(KEY, '123456', server.post)
        self.assertFalse(server.finished)

    def test_changed_host_key_never_enrolls(self):
        server = Server(tamper=True)
        with self.assertRaises(ValueError):
            meowdbg.pairing_exchange(KEY, '123456', server.post)
        self.assertFalse(server.finished)

    def test_lan_and_cloud_require_same_pinned_host_key(self):
        with tempfile.TemporaryDirectory() as d, patch.object(meowdbg, 'STATE', Path(d)):
            server = Server()
            profile = meowdbg.pairing_exchange(KEY, '123456', server.post)
            meowdbg.private_write(Path(d) / 'devices' / f'{DEVICE}.json', json.dumps(profile))
            meowdbg.private_write(Path(d) / 'selected', DEVICE)
            lan = meowdbg.ssh_base('192.0.2.1')
            meowdbg.private_write(Path(d) / 'cloud', DEVICE)
            cloud = meowdbg.ssh_base('cloud')
            for command in (lan, cloud):
                self.assertIn('StrictHostKeyChecking=yes', command)
                self.assertIn(f'HostKeyAlias=meow-{DEVICE}', command)
            self.assertEqual((Path(d) / 'hosts' / DEVICE).read_text(), f'meow-{DEVICE} {profile["host_key"]}\n')
            self.assertEqual((Path(d) / 'devices' / f'{DEVICE}.json').stat().st_mode & 0o777, 0o600)

    def test_no_legacy_trust_on_first_use_fallback(self):
        with tempfile.TemporaryDirectory() as d, patch.object(meowdbg, 'STATE', Path(d)):
            with self.assertRaises(SystemExit):
                meowdbg.ssh_base('192.0.2.1')


if __name__ == '__main__':
    if '--interop' in sys.argv:
        def post(step, body):
            print(json.dumps({'step': step, 'body': body}), flush=True)
            return json.loads(sys.stdin.readline())
        print(json.dumps(meowdbg.pairing_exchange(KEY, '123456', post)), flush=True)
    else:
        unittest.main()
