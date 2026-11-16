#!/usr/bin/env python3
"""meowdbg: the network debug bridge to a meow dongle (Settings > Developer > Network debugging).

The dongle runs Dropbear (root, public keys only) while the toggle is on. Pair once with the
6-digit code shown after choosing Pair developer computer; the dongle stores your key on /data, so pairing
survives reboots and re-flashes that keep /data.

  meowdbg pair HOST [CODE]        enrol this Mac's key (creates ~/.ssh/meowdbg_ed25519)
  meowdbg shell                 interactive root shell
  meowdbg run CMD...            run a command, exit with its status
  meowdbg push LOCAL REMOTE     copy a file to the dongle (keeps the executable bit)
  meowdbg pull REMOTE LOCAL     copy a file from the dongle
  meowdbg log [ui|netd|appd|init|wpa|kernel]   follow a log (default netd)
  meowdbg forward LPORT RPORT   forward localhost:LPORT to the dongle's port RPORT
  meowdbg use-cloud DEVICE_ID   reach the dongle through the meowcast relay from now on (any
                                network; the id is under Settings > Developer). Pair once first
                                over USB or a LAN where the dongle is reachable.
  meowdbg use-lan [HOST]        back to direct connections

HOST is remembered after pairing (--host overrides). Pairing needs spake2 (requirements.txt).
"""
import argparse
import getpass
import base64
import hashlib
import select
import socket
import ssl
import time
import struct
import hmac
import json
import os
import re
import shlex
import subprocess
import sys
import urllib.request
from pathlib import Path

KEY = Path.home() / '.ssh' / 'meowdbg_ed25519'


def _relay():
    """The relay host: $MEOW_RELAY_HOST, else MEOW_RELAY_HOST in the repository's config.env."""
    host = os.environ.get('MEOW_RELAY_HOST', '')
    conf = Path(os.environ.get('MEOW_CONFIG', Path(__file__).resolve().parents[1] / 'config.env'))
    if not host and conf.is_file():
        for line in conf.read_text().splitlines():
            if line.startswith('MEOW_RELAY_HOST='):
                host = line.split('=', 1)[1].strip().strip('"\'')
    return host


RELAY = _relay()
STATE = Path.home() / '.config' / 'meowdbg'
LOGS = {
    'ui': '/run/ui.log',
    'netd': '/run/netd.log',
    'appd': '/run/appd.log',
    'init': '/run/init.log',
    'wpa': '/run/netd/wpa-wlan0.log',
}


def die(msg):
    print(f'meowdbg: {msg}', file=sys.stderr)
    sys.exit(1)


def ensure_key():
    if not KEY.exists():
        KEY.parent.mkdir(mode=0o700, exist_ok=True)
        subprocess.run(['ssh-keygen', '-q', '-t', 'ed25519', '-N', '', '-C', f'meowdbg@{os.uname().nodename}', '-f', str(KEY)], check=True)
    return KEY.with_suffix('.pub').read_text().strip()


def host(args):
    if args.host:
        return args.host
    if cloud_device():
        return 'cloud'
    f = STATE / 'host'
    if f.exists():
        return f.read_text().strip()
    die('no dongle yet: meowdbg pair HOST [CODE]')


def cloud_device():
    f = STATE / 'cloud'
    return f.read_text().strip() if f.exists() else None


def private_write(path, text):
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    os.chmod(path.parent, 0o700)
    tmp = path.with_suffix('.tmp')
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, 'w') as f:
        os.fchmod(f.fileno(), 0o600)
        f.write(text)
    os.replace(tmp, path)


def profile(device=None):
    device = device or cloud_device() or ((STATE / 'selected').read_text().strip() if (STATE / 'selected').exists() else '')
    if not re.fullmatch(r'[0-9a-f]{32}', device):
        die('pair securely first: meowdbg pair HOST')
    try:
        result = json.loads((STATE / 'devices' / f'{device}.json').read_text())
        if result['device'] != device or not re.fullmatch(r'[0-9a-f]{64}', result['token']):
            raise ValueError('invalid credential')
        validate_ssh_key(result['host_key'])
        return result
    except (OSError, ValueError, KeyError):
        die('no secure pairing for this device; run meowdbg pair HOST')


def ssh_base(h, extra=()):
    dev = cloud_device()
    paired = profile(dev)
    device = paired['device']
    alias = f'meow-{device}'
    via = []
    if dev:
        command = shlex.join([sys.executable, str(Path(__file__).resolve()), 'proxy', dev])
        via = ['-o', f'ProxyCommand={command}']
        h = alias
    # Pin one authenticated device key for both transports; never learn a key from
    # ssh-keyscan, the relay, or an unauthenticated first SSH connection.
    known = STATE / 'hosts' / device
    private_write(known, f'{alias} {paired["host_key"]}\n')
    return ['ssh', '-i', str(KEY), '-o', 'IdentitiesOnly=yes', '-o', f'UserKnownHostsFile={known}',
            '-o', 'GlobalKnownHostsFile=/dev/null', '-o', f'HostKeyAlias={alias}', '-o', 'CheckHostIP=no',
            '-o', 'StrictHostKeyChecking=yes', '-o', 'ConnectTimeout=15', '-o', 'ServerAliveInterval=10',
            *via, *extra, f'root@{h}']


def proxy(device):
    """stdin/stdout <-> wss://RELAY/dev/DEVICE (binary WebSocket frames): ssh's ProxyCommand."""
    if not re.fullmatch(r'[0-9a-f]{32}', device):
        die('invalid device ID')
    token = profile(device)['token']
    ctx = ssl.create_default_context()
    raw = socket.create_connection((RELAY, 443), timeout=15)
    s = ctx.wrap_socket(raw, server_hostname=RELAY)
    key = base64.b64encode(os.urandom(16)).decode()
    s.sendall(f'GET /dev/{device} HTTP/1.1\r\nHost: {RELAY}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n'
              f'Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\nAuthorization: Bearer {token}\r\nUser-Agent: meowdbg\r\n\r\n'.encode())
    buf = b''
    while b'\r\n\r\n' not in buf:
        chunk = s.recv(4096)
        if not chunk:
            die('relay closed the connection')
        buf += chunk
        if len(buf) > 16384:
            die('relay handshake too large')
    head, buf = buf.split(b'\r\n\r\n', 1)
    status = head.split(b'\r\n')[0].decode(errors='replace')
    if ' 101 ' not in status:
        die(f'relay: {status} {buf[:200].decode(errors="replace")}')
    headers = dict(line.split(b':', 1) for line in head.split(b'\r\n')[1:] if b':' in line)
    headers = {k.strip().lower(): v.strip() for k, v in headers.items()}
    expected = base64.b64encode(hashlib.sha1((key + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').encode()).digest())
    if headers.get(b'sec-websocket-accept') != expected:
        die('invalid relay WebSocket handshake')
    s.settimeout(None)

    def send(op, payload):
        mask = os.urandom(4)
        n = len(payload)
        hdr = bytes([0x80 | op]) + (bytes([0x80 | n]) if n < 126 else bytes([0x80 | 126]) + struct.pack('>H', n) if n < 65536
                                    else bytes([0x80 | 127]) + struct.pack('>Q', n))
        s.sendall(hdr + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(payload)))

    out = sys.stdout.buffer
    stdin_open = True
    tunnel_ready = False
    buffered = bool(buf)
    deadline = time.monotonic() + 10
    next_send = time.monotonic()
    while True:
        if not tunnel_ready and time.monotonic() > deadline:
            die('dongle did not authorise the debug tunnel')
        r = [s] + ([sys.stdin] if stdin_open and tunnel_ready else [])
        ready = [s] if s.pending() or buffered else select.select(r, [], [], 1 if not tunnel_ready else 30)[0]
        if not ready:
            send(9, b'')  # keep idle sessions alive through NAT
            continue
        if sys.stdin in ready:
            data = os.read(0, 16384)
            if not data:
                stdin_open = False
                send(8, struct.pack('>H', 1000))
            else:
                # Pace bulk copies below the relay's per-tunnel budget.
                next_send = max(next_send, time.monotonic()) + len(data) / 524288
                time.sleep(max(0, next_send - time.monotonic()))
                send(2, data)
        if s in ready:
            if buffered:
                buffered = False
            else:
                chunk = s.recv(65536)
                if not chunk:
                    return
                buf += chunk
            while len(buf) >= 2:
                op, n, i = buf[0] & 0x0f, buf[1] & 0x7f, 2
                if n == 126:
                    if len(buf) < 4:
                        break
                    n, i = struct.unpack('>H', buf[2:4])[0], 4
                elif n == 127:
                    if len(buf) < 10:
                        break
                    n, i = struct.unpack('>Q', buf[2:10])[0], 10
                if n > 16384:
                    die('relay frame too large')
                if len(buf) < i + n:
                    break
                payload, buf = buf[i:i + n], buf[i + n:]
                if op == 1:
                    message = json.loads(payload)
                    if message.get('t') == 'closed':
                        die(f'debug tunnel closed: {message.get("reason", "permission changed")}')
                    if message.get('t') == 'ready':
                        tunnel_ready = True
                elif op == 2:
                    if not tunnel_ready:
                        die('data before tunnel authorisation')
                    out.write(payload)
                    out.flush()
                elif op == 9:
                    send(10, payload)
                elif op == 8:
                    if len(payload) >= 2:
                        code = struct.unpack('>H', payload[:2])[0]
                        if code != 1000:
                            print(f'meowdbg: relay closed the tunnel: {code} {payload[2:].decode(errors="replace")}', file=sys.stderr)
                    return


def validate_ssh_key(key):
    fields = key.split()
    if len(fields) < 2 or fields[0] != 'ssh-ed25519':
        raise ValueError('expected an Ed25519 SSH key')
    blob = base64.b64decode(fields[1], validate=True)
    if len(blob) != 51 or blob[:19] != b'\x00\x00\x00\x0bssh-ed25519\x00\x00\x00\x20':
        raise ValueError('invalid SSH key')
    return ' '.join(fields[:2])


def pairing_proof(secret, role, payload):
    return hmac.digest(secret, f'meowdbg-v2/{role}\n{payload}'.encode(), 'sha256').hex()


def pairing_exchange(pub, code, post):
    from spake2 import SPAKE2_A
    state = SPAKE2_A(code.encode(), idA=b'meowdbg-v2/client', idB=b'meowdbg-v2/dongle')
    r = post('start', {'key': pub, 'message': state.start().hex()})
    message = bytes.fromhex(r['message'])
    if len(message) != 33:
        raise ValueError('invalid pairing message')
    secret = state.finish(message)
    payload = r['payload']
    if not hmac.compare_digest(pairing_proof(secret, 'server', payload), r['proof']):
        raise ValueError('wrong TV code or intercepted pairing; reopen pairing on the TV')
    p = json.loads(payload)
    if p.get('version') != 2 or validate_ssh_key(p['key']) != validate_ssh_key(pub) or p['session'] != r['session']:
        raise ValueError('pairing keys or session changed')
    if not re.fullmatch(r'[0-9a-f]{32}', p['device']):
        raise ValueError('invalid device identity')
    host_key = validate_ssh_key(p['host_key'])
    r = post('finish', {'session': p['session'], 'proof': pairing_proof(secret, 'client', payload)})
    if not hmac.compare_digest(pairing_proof(secret, 'complete', payload), r['proof']):
        raise ValueError('pairing completion could not be authenticated')
    return {'device': p['device'], 'host_key': host_key, 'token': pairing_proof(secret, 'admission', payload)}


def pair(args):
    try:
        import spake2  # noqa: F401; only enrollment needs this dependency
    except ImportError:
        die('pairing needs spake2: install tools/requirements.txt in a Python virtual environment')
    if not re.fullmatch(r'[A-Za-z0-9.-]+(?::[0-9]{1,5})?', args.pair_host):
        die('use a dongle hostname or IPv4 address, optionally with :PORT')
    pub = ensure_key()
    code = (args.code or getpass.getpass('Six-digit code from Pair developer computer on the TV: ')).replace(' ', '')
    if not re.fullmatch(r'[0-9]{6}', code):
        die('the TV code has six digits')
    # No redirects or environment proxies during local enrollment.
    class NoRedirect(urllib.request.HTTPRedirectHandler):
        def redirect_request(self, *args, **kwargs):
            return None
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
    def post(step, body):
        req = urllib.request.Request(f'http://{args.pair_host}/api/debug/pair/{step}', data=json.dumps(body).encode(), headers={'Content-Type': 'application/json'})
        try:
            with opener.open(req, timeout=10) as response:
                raw = response.read(8193)
                if len(raw) > 8192:
                    raise ValueError('pairing response too large')
                result = json.loads(raw)
        except urllib.error.HTTPError as e:
            result = json.loads(e.read(8192) or b'{}')
        if not result.get('ok'):
            raise ValueError(f'pairing refused: {result.get("error", "unknown")}; choose Pair developer computer on the TV')
        return result
    try:
        paired = pairing_exchange(pub, code, post)
    except (OSError, ValueError, KeyError) as e:
        die(str(e))
    paired['host'] = args.pair_host.split(':')[0]
    private_write(STATE / 'devices' / f'{paired["device"]}.json', json.dumps(paired) + '\n')
    private_write(STATE / 'selected', paired['device'] + '\n')
    private_write(STATE / 'host', paired['host'] + '\n')
    (STATE / 'cloud').unlink(missing_ok=True)
    print(f'Paired securely with {args.pair_host}; SSH host key pinned for LAN and cloud.')
    print(f'Cloud: enable Allow through cloud on the TV, then meowdbg use-cloud {paired["device"]}')


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument('--host', help='dongle address (default: the paired one)')
    sub = p.add_subparsers(dest='cmd', required=True)
    s = sub.add_parser('pair')
    s.add_argument('pair_host')
    s.add_argument('code', nargs='?', help='TV code (default: secure interactive prompt)')
    sub.add_parser('shell')
    s = sub.add_parser('run')
    s.add_argument('command', nargs=argparse.REMAINDER)
    s = sub.add_parser('push')
    s.add_argument('local')
    s.add_argument('remote')
    s = sub.add_parser('pull')
    s.add_argument('remote')
    s.add_argument('local')
    s = sub.add_parser('log')
    s.add_argument('which', nargs='?', default='netd')
    s = sub.add_parser('forward')
    s.add_argument('lport', type=int)
    s.add_argument('rport', type=int)
    s = sub.add_parser('use-cloud')
    s.add_argument('device')
    s = sub.add_parser('use-lan')
    s.add_argument('lan_host', nargs='?')
    s = sub.add_parser('proxy')
    s.add_argument('device')
    args = p.parse_args()

    if args.cmd == 'pair':
        return pair(args)
    if args.cmd == 'proxy':
        return proxy(args.device)
    if args.cmd == 'use-cloud':
        if not re.fullmatch(r'[0-9a-f]{32}', args.device):
            die('the device id is 32 hex characters (Settings > Developer)')
        profile(args.device)
        private_write(STATE / 'cloud', args.device + '\n')
        private_write(STATE / 'selected', args.device + '\n')
        return print(f'meowdbg now goes through {RELAY} to {args.device}')
    if args.cmd == 'use-lan':
        paired = profile()
        (STATE / 'cloud').unlink(missing_ok=True)
        private_write(STATE / 'host', (args.lan_host or paired['host']) + '\n')
        return print('meowdbg now connects directly')
    if not KEY.exists():
        die('not paired: meowdbg pair HOST [CODE]')
    h = host(args)
    if args.cmd == 'shell':
        os.execvp('ssh', ssh_base(h, ['-t']))
    if args.cmd == 'run':
        if not args.command:
            die('run what?')
        # One argument is a shell command line ('a; b | c'); several are quoted words.
        line = args.command[0] if len(args.command) == 1 else ' '.join(shlex.quote(c) for c in args.command)
        sys.exit(subprocess.run([*ssh_base(h), line]).returncode)
    if args.cmd == 'push':
        src = Path(args.local)
        mode = '755' if os.access(src, os.X_OK) else '644'
        q = shlex.quote(args.remote)
        with src.open('rb') as f:
            r = subprocess.run([*ssh_base(h), f'cat > {q}.part && chmod {mode} {q}.part && mv {q}.part {q}'], stdin=f)
        sys.exit(r.returncode)
    if args.cmd == 'pull':
        with open(args.local, 'wb') as f:
            r = subprocess.run([*ssh_base(h), f'cat {shlex.quote(args.remote)}'], stdout=f)
        sys.exit(r.returncode)
    if args.cmd == 'log':
        cmd = 'cat /dev/kmsg' if args.which == 'kernel' else f'tail -n 200 -f {shlex.quote(LOGS.get(args.which, args.which))}'
        os.execvp('ssh', ssh_base(h, ['-t']) + [cmd])
    if args.cmd == 'forward':
        print(f'localhost:{args.lport} -> dongle:{args.rport} (Ctrl-C to stop)')
        os.execvp('ssh', ssh_base(h, ['-N', '-L', f'{args.lport}:127.0.0.1:{args.rport}']))


if __name__ == '__main__':
    main()
