# Architecture

```text
 your Cloudflare account                         the device
 ┌──────────────────────────┐              ┌───────────────────────────────────────────────┐
 │ store   signed apps,     │◄──HTTPS──────┤ appd        installs, sandboxes, permissions   │
 │         wallpapers, fonts│              │  ├ apps     Store, yours, apps-extra ...        │
 │ relay   one object per   │◄──WSS────────┤  └ services meow-net (HTTP/TLS), meow-image     │
 │         device           │              │ meow-netd   Wi-Fi, pairing, cloud link, time    │
 │ pilot   phone remote app │              │ launcher    home screen, settings (GLES2, KMS)  │
 └────────────▲─────────────┘              │ thermald, buttond, BusyBox init                 │
              │ HTTPS/WSS                  │ Linux 7.3 + Mesa/Lima, all in one initramfs     │
            phone                          └───────────────────────────────────────────────┘
```

## Layers

| Layer | Directory | Depends on the board? |
| --- | --- | --- |
| Boot code, flash layout, flashing tools, device trees, kernel config | `boards/<board>/` | It is the board |
| Kernel and initramfs build, init, system daemons | `os/` | Through `board.sh` and the board's kernel hook only |
| Wi-Fi driver and userspace, TLS client | `wifi/`, `cloud/tls` | The driver is for one USB chip (RTL8733BU) |
| App manager, SDK, network daemon, services, apps | `apps/` | No: Linux interfaces (DRM/KMS, V4L2 stateless decoding, ALSA, evdev) |
| Launcher | `ui/` | No: GLES2 over GBM/KMS |
| Store, relay, pilot | `cloud/` | No |

The userspace is built for ARMv7 hard-float today (`armv7-unknown-linux-gnueabihf` in
`ui/` and `apps/`, `arm-linux-gnueabihf` in `os/` and `wifi/`). A 64-bit board needs those
targets made configurable; see [boards/README.md](../boards/README.md#what-is-still-tied-to-the-first-board).

## Storage

Nothing executable is stored on flash except the firmware image itself. The device has:

- the **firmware image** (kernel + initramfs), read-only, replaced as a whole;
- **`/data`**, a small flash partition (JFFS2) for settings, signed app manifests, permission
  grants and each app's quota-limited storage;
- **RAM** for everything else: app payloads are downloaded and verified at their first launch
  after each boot, wallpapers and full fonts when needed.

## Trust

| Who | Trusted for | How it is limited |
| --- | --- | --- |
| Firmware image | Everything | You build and flash it. There is no over-the-air firmware update |
| Store signing key | Which apps and services the device installs from the built-in store | Public half in the read-only initramfs; every manifest and the asset index are signed, every file is hashed |
| Store host, network | Nothing | Can only withhold. TLS (pinned roots) adds privacy |
| Added repositories | Apps signed by the one key the user confirmed | Cannot carry system services or apps that manage other apps |
| An app | What its granted permissions say | Own user, namespaces, cgroup, Landlock, seccomp; network only through the network service and only to hosts in its manifest |
| Relay | Passing messages | Stores a hash of the device key; pairing codes are checked on the device; phone tokens are compared as hashes on the device |
| A paired phone | Remote-control keys, text, volume, launching apps | Pairing needs the code shown on the TV |
| A paired developer | A root shell over SSH through the relay | Paired on the TV, time-limited lease, end-to-end SSH ([cloud/DEBUG-SECURITY.md](../cloud/DEBUG-SECURITY.md)) |
| USB cable (LAB images) | A root shell without authentication | Physical access; build with `LAB=0` to leave the lab tools out |

Details: [apps/README.md](../apps/README.md) (sandbox and permissions),
[cloud/README.md](../cloud/README.md) (relay protocol), [wifi/README.md](../wifi/README.md)
(pairing and Wi-Fi setup).
