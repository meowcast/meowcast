# Developer debugging

Developer SSH is root access. It is off after every boot and daemon restart. Turning it
on grants a one-hour lease, with USB access and separate, initially-off LAN and cloud
switches. Disabling access, expiry, or revoking a computer ends the SSH process subtree
and closes cloud tunnels. Access changes also end existing sessions so no connection
can retain an earlier policy.

## Pairing UX

1. On the TV, open **Settings → Developer → Network debugging**.
2. Choose **Pair developer computer**. A fresh six-digit code is shown for two minutes.
3. On the computer, run `meowdbg pair HOST` using the address shown on the TV. Enter the
   code at the hidden prompt. If the router isolates Wi-Fi clients, use the USB address.
4. The CLI enrolls the computer's Ed25519 key and pins the dongle's SSH host key. Pairing
   closes immediately after success. Future sessions require no code while debugging
   is enabled; LAN and cloud always use this same host-key pin.
5. For cloud access, enable **Allow through cloud**, then run `meowdbg use-cloud DEVICE_ID`.
   For direct Wi-Fi SSH, enable **Allow on local network**. USB does not require that switch.

The CLI's one-time Python environment setup (SSH itself must already be installed):

```sh
python3 -m venv .venv-meowdbg
.venv-meowdbg/bin/pip install -r tools/requirements.txt
.venv-meowdbg/bin/python tools/meowdbg.py pair 192.168.2.2
```

Each enrolled computer is listed in Developer settings. Selecting it offers **Revoke**
with confirmation; this removes its SSH key and cloud credential and ends current debug
sessions. At most eight computers are enrolled. Re-pairing the same SSH key rotates its
cloud credential. Losing the computer's private key requires revoking and pairing again.

## Protocol and boundaries

- Enrollment uses the interoperable `spake2` libraries (Rust 0.4, Python 0.9), with
  Ed25519 SPAKE2 roles A/client and B/dongle and versioned identities. The six-digit
  password is never sent, nor is a reusable password-derived MAC. Each exchange permits
  one online guess, with five starts per budget, a 30-second lockout, one outstanding
  exchange, and a 30-second exchange timeout. Reopening the TV window does not reset
  the guess budget. Successful enrollment resets it for a fresh pairing code.
  The dongle explicitly validates canonical, nonidentity prime-order curve points;
  the Python library performs the corresponding subgroup checks.
- The PAKE result authenticates a canonical payload containing the session, protocol
  version, computer's SSH public key, dongle's SSH public key, and device ID. HMAC-SHA256
  uses distinct server/client/completion labels for mutual key confirmation. The client
  enrolls only after verifying the server proof; the dongle installs a key only after
  verifying the client proof. Sessions are single use and invalidated by policy changes.
- A distinct `admission` label derives a 256-bit cloud bearer credential from the PAKE
  result and payload. The credential is never sent over enrollment HTTP. The computer
  stores it in a mode-0600 per-device profile and sends it in an Authorization header
  over verified TLS. The dongle and relay use only its SHA-256 for admission checks.
  Both an admitted cloud credential and the enrolled SSH private key are required.
- The relay accepts no browser-origin debug upgrades. It needs a fresh dongle policy,
  renewed every five seconds and expiring within fifteen seconds. The dongle independently
  checks cloud permission, expiry and enrollment before connecting a fixed loopback SSH
  port; it never accepts an arbitrary target address or port. The CLI waits for dongle
  approval before passing SSH bytes.
- Each device has at most two tunnels. Client binary messages are limited to 16 KiB,
  with a 1 MiB/s per-tunnel budget. The CLI paces bulk copies at 512 KiB/s. Oversized
  input closes only its tunnel, before adding the four-byte channel header. The dongle
  caps tunnels independently, uses a bounded 64-message queue, write timeouts, and shuts
  down cloned sockets when a tunnel or cloud session is dropped. Pending tunnel opens
  time out after ten seconds; idle tunnels expire after two minutes. SSH keepalives
  preserve legitimate idle sessions.
- Dropbear starts inside a dedicated cgroup v2 before exec. `cgroup.kill` terminates
  forked SSH sessions, shells and port forwards on disable, policy changes, expiry or
  daemon shutdown. Debugging refuses to start without this kernel support or a valid
  host key. LAN SSH is not bound until explicitly allowed; loopback and the current
  USB address are available while debugging is enabled.

The relay remains trusted for availability and the phone remote. SSH encryption and
the pinned host key protect developer session contents against relay impersonation.
A computer with root SSH access can modify the firmware and security policy; revocation
is not intended to undo a prior full device compromise.

## Migration and rollout

Legacy developer enrollment is intentionally invalidated. The dongle retains its SSH
host key and creates its v2 developer store, but rewrites `authorized_keys` from that
store and ignores the old persistent `enabled` flag. Pair each developer computer once
using the new flow. Existing phone pairing is unaffected.

Build and deploy the relay and new firmware together. The firmware userspace build now
includes `dropbearkey`, required to authenticate the host key during enrollment. Reboot
into the new firmware, rather than replacing only the daemon while an old SSH listener
is still running. An old relay cannot carry the new protocol; new firmware still checks
admission locally, and the new relay rejects old anonymous developer clients.

Before rollout on hardware, exercise LAN/USB binding, the two-minute TV flow, off/expiry,
and revocation with an open shell and port forward. Local automated checks cover PAKE
key confirmation, replay, expiry, Python/Rust interoperability, host pinning, cloned
socket shutdown, and the real Workers runtime's admission and tunnel isolation.

```sh
cd cloud/relay
npm run check
npm run test:security
# From the repository root, with the pairing venv:
.venv-meowdbg/bin/python -m unittest discover -s tools -p test_meowdbg.py
```
