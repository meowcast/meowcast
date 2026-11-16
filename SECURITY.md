# Security

## Reporting a problem

Please do not open a public issue for a vulnerability. Use GitHub's private reporting on this
repository (**Security → Report a vulnerability**). Say what is affected, how to reproduce it
and what an attacker gains. You will get an answer within a week. There is no bounty.

In scope: the app sandbox and permission system (`apps/appd`), signature and hash checking of
apps and assets, pairing and the relay protocol (`apps/netd`, `cloud/relay`), the developer
bridge, the TLS client (`cloud/tls`), and the flash tools' write protection.

## What you should know before using it

- **There are no automatic firmware updates.** You rebuild and reflash to get fixes.
- **LAB builds are for the bench.** The default build of the first board (`LAB=1`) gives a
  root shell on the USB cable without authentication, makes clock and register debugfs
  writable, and reboots into the boot ROM's USB mode. Build with `LAB=0` for a device in use.
- **Physical access is full access.** The boot ROM's USB mode can read and rewrite the flash;
  there is no secure boot. Pairing tokens and Wi-Fi credentials are stored on `/data`
  unencrypted.
- **Your store key is the root of app trust** for your devices. Keep it offline or at least
  out of any repository.
- **The relay operator (you) can see** key presses and status messages of the devices using
  it, and can deny service. It cannot pair a phone or open a developer session: those are
  decided on the device.
- **Apps with the `lan` permission listen on the local network** (the casting receivers of
  [apps-extra](https://github.com/meowcast/apps-extra), for example). They are sandboxed, and largely untested against
  hostile input.

The trust model is described in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md#trust).
