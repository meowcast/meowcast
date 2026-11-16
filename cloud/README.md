# meowcast cloud: the pilot app and the relay

The phone and the dongle often cannot reach each other directly. Many routers isolate Wi-Fi
clients from each other: on the test network (14 Nov) neither the phone nor the Mac could reach the
dongle at 192.168.1.15. A phone may also be on mobile data. So the pilot app runs from Cloudflare,
and a relay connects it to the dongle, which keeps an outgoing connection open.

```
phone (MEOW Pilot, https://pilot.example.org)
   │  POST /api/pair, wss /pilot/<device>?k=TOKEN
   ▼
relay Worker  https://relay.example.org   ── Durable Object "Device" per dongle
   ▲
   │  wss /device/<device>  (X-Device-Key), over meow-tls (BearSSL, pinned roots)
dongle: meow-netd cloud.rs
```

| Part | Where | Deploy |
|---|---|---|
| Relay: Worker + Durable Object (SQLite-backed, hibernatable WebSockets) | `relay/` | `sh deploy.sh relay` |
| Pilot app: static-assets Worker, single-page app | `pilot/` (`wrangler.jsonc`, `dist-cloud`) | `sh deploy.sh pilot` |
| Store: apps + assets (wallpaper, full fonts), static-assets Worker `meowcast-store` | `store/` (`wrangler.jsonc`, `www/`) | `sh deploy.sh store` (see [store/README.md](store/README.md)) |
| TLS client on the dongle | `tls/meow-tls.c` + `tls/roots.pem` | built by `../wifi/build-userspace.sh` |
| Dongle side | `../apps/netd/src/cloud.rs` | firmware |

Deploy all three to your own Cloudflare account with `sh deploy.sh` (or `./meow cloud deploy`
from the repository root): names and addresses come from `config.env`, and
[../docs/CLOUD.md](../docs/CLOUD.md) walks through it. The host names in this document
(`relay.example.org`, `pilot.example.org`, `store.example.org`) stand for yours. The pilot app
is a static-assets Worker rather than a Pages project.

## Trust and secrets

- **Device identity.** On first use the dongle creates `/data/net/cloud/id` (128 bits) and
  `/data/net/cloud/key` (256 bits). The id goes into the TV's QR link. The key is sent only over
  verified TLS. The relay stores SHA-256(key) on the device's first connection (trust on first
  use) and refuses other keys for that id afterwards.
- **Pairing stays on the dongle.** The relay forwards the 6-digit code and the dongle answers with
  the same rules as on the LAN: only while the TV shows the code, a new code after every success
  and after 5 wrong tries, and a 30 s lockout.
- **Pilot tokens.** The relay receives the token during pairing and the phone WebSocket
  handshake, but does not persist it. It forwards SHA-256(token), and the dongle compares that
  with `/data/net/pilots`. Unknown tokens are closed with 4401, and their keys are never forwarded.
- **What the relay does see.** Key presses and the status line, inside TLS on both legs.
- **CORS.** `/api/*` answers browsers only from `PILOT_ORIGINS` (the app's origin and the
  local Vite dev server).
- **TLS on the dongle.** `meow-tls` (BearSSL git `7bea48e`) checks the chain against five pinned
  roots: GTS Root R1 and R4, GlobalSign Root CA, ISRG Root X1 and X2 (`tls/roots.pem`, exported
  from macOS's system roots; SHA-256 fingerprints in the file). Today's `*.workers.dev` chain is
  WE1 → GTS Root R4 → GlobalSign Root CA. It also checks the host name and needs the clock set by
  NTP; the cloud link waits for that. Tested: self-signed, expired, untrusted-root and
  wrong-host certificates are refused.

## Protocol (JSON text frames)

| From → to | Message |
|---|---|
| relay → dongle | `{t:"hello", pid, hash}`: a phone connected; `hash` = SHA-256(token) |
| dongle → relay | `{t:"auth", pid, ok}`: unknown tokens are closed with 4401 |
| phone → relay → dongle | `{t:"key", k}`: up, down, left, right, ok, back, home; only from authorised phones |
| relay → dongle | `{t:"pair", rid, code, name}` |
| dongle → relay | `{t:"pair-result", rid, ok, token \| error}` |
| dongle → relay → phones | `{t:"status", dongle}` (the launcher is up), plus `online` added by the relay |
| phone → relay → dongle | anything else with a `t`: wrapped as `{t:"msg", pid, m}` (pilot protocol 2: volume, apps, text, heartbeat; see [../wifi/README.md](../wifi/README.md)) |
| dongle → relay → phone(s) | `{t:"pilot", pid?, m}`: `m` goes to that phone, or to every authorised phone without `pid` |
| relay → dongle | `{t:"bye", pid}`: an authorised phone's socket closed |
| relay → phone | `{t:"unpaired"}`, then close 4401: the dongle sent `auth` with `ok:false` for a phone it had accepted (*Unpair all phones*) |

The relay does not look inside `msg` and `pilot`, so new pilot features need no relay deploy.
Wrapping keeps a phone from forging the relay's own messages (`hello`, `pair`, `tunnel-open`).
Messages are limited to 4096 bytes from a phone and 16 384 from the dongle.

**Compatibility.** A dongle with firmware from before pilot protocol 2 ignores `msg` and `bye`
and never sends `pilot`; the pilot app then gets no `state` and shows the keys only. A pilot from
before it sends only `key`. Deploy the relay first, then the pilot app; the firmware can follow
at any time.

HTTP: `GET /api/info?device=ID` → `{online, dongle}`; `POST /api/pair {device, code, name}` →
`{ok, token}`, or 403 `bad_code`/`not_open`/`locked`, or 503 `offline`.

## On the dongle

- `/etc/meow/cloud.conf` (from `build.sh`; `MEOW_RELAY_HOST`/`MEOW_PILOT_URL` override at build time;
  `MEOW_RELAY_HOST` empty = no cloud link) and `/data/net/cloud.conf` (per device, wins):
  `relay=HOST`, `app=URL`, `plain=HOST:PORT` (unencrypted, for a `wrangler dev` on the Mac).
- The link starts once there is a default route (Wi-Fi or USB) and the clock is set. It reconnects
  with backoff from 2 s to 60 s. It pings every 25 s and sends status every 30 s.
- `/run/netd/state` says `cloud=off|waiting|connecting|online`. While online, the TV's pairing QR
  is `<app>/?d=<id>&pair=<code>`. Otherwise it is the dongle's LAN address.
- The dongle's own web page (`pilot/setup/index.html`, 3.8 KB gzipped) is the captive-portal
  Wi-Fi setup and a link to the cloud pilot. The full React app is no longer in the firmware,
  which saved about 90 KB.

## Tests

`relay/test/e2e.mjs [URL]` plays a dongle and a phone against `wrangler dev` (default) or the
deployed relay:

- pairing while offline → 503;
- an impostor device key is refused;
- a wrong code → 403, the right code → token;
- keys are relayed;
- a bad token gets 4401 and its keys are dropped;
- info shows online, then offline.

Passed locally and against the deployed relay on 14 Nov 2026.

`relay/test/pilot.mjs [URL]` checks pilot protocol 2 the same way (locally, 14 Nov 2026):

- `msg`/`pilot` envelopes, replies to one phone, broadcasts to all;
- forged relay messages from a phone arrive wrapped;
- nothing from or to an unauthorised phone; `bye` when a phone leaves; `unpaired` + 4401;
- a dongle that only knows keys with a new pilot, and a new dongle with a keys-only pilot.

## Secure developer bridge

See [Developer debugging](DEBUG-SECURITY.md) for the TV/CLI pairing flow, protocol,
access leases, developer revocation, tests, and coordinated relay rollout.
`/dev/<id>` now requires a securely paired developer credential in the Authorization
header, a fresh dongle permission lease, and end-to-end SSH authentication. Device IDs
are not credentials. Old anonymous debug clients must re-pair with the updated CLI.
