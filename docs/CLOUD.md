# Your cloud side

The firmware talks to three small services. There is no shared project server: you run your
own, and the addresses are built into your firmware image.

| Service | What it does | Code |
| --- | --- | --- |
| **Store** | A static site: signed apps, wallpapers, full fonts. The device checks every signature and hash, so the host is not trusted | `cloud/store` |
| **Relay** | Connects a phone to a device when they cannot reach each other directly (client isolation, mobile data). One Durable Object per device; it sees key presses and status, never credentials | `cloud/relay` |
| **Pilot** | The phone remote, a static web app | `cloud/pilot` |

All three run as Cloudflare Workers and fit in the free plan for personal use (the relay
needs Durable Objects with SQLite storage, which the free plan includes).

## Deploy

```sh
(cd cloud/relay && npm ci && npx wrangler login)    # once
./meow build apps        # packs and signs the store with your key (needs ./meow fetch first)
./meow cloud deploy      # relay, pilot, store
./meow build             # the firmware now carries your addresses
```

`./meow cloud deploy`:

1. deploys the relay under `MEOW_CF_RELAY_NAME` and allows your pilot app's origin to call it;
2. builds the pilot app against your relay and deploys it under `MEOW_CF_PILOT_NAME`;
3. checks every file of `cloud/store/www` against the signed index, deploys it under
   `MEOW_CF_STORE_NAME`, and compares the live index with the local one;
4. writes the addresses it learned into `config.env` (`MEOW_RELAY_HOST`, `MEOW_PILOT_URL`,
   `MEOW_STORE_URL`), unless you had set them.

`./meow cloud relay|pilot|store` deploys one of them; `--dry-run` builds without uploading.

**Own domain.** Give the Workers routes or custom domains in the Cloudflare dashboard, then set
the three addresses in `config.env` yourself and deploy again. The device verifies TLS against
five pinned roots (Google Trust Services, GlobalSign, Let's Encrypt: `cloud/tls/roots.pem`), so
the certificate must chain to one of them. Cloudflare's do.

**Another host.** The store is plain files: any static host with HTTPS works
(`cloud/store/www`, see [cloud/store/README.md](../cloud/store/README.md)). The relay needs
WebSockets and per-device state; it is about 500 lines of TypeScript written for Workers and
Durable Objects ([cloud/README.md](../cloud/README.md) has the protocol).

## The signing key

`./meow build apps` creates `MEOW_STORE_KEY` on first use and bakes its public half into the
firmware (`/etc/appd/keys`). From then on your devices install only what that key signed.
Keep the private key out of the repository and back it up.

## Updating

- **Apps and assets**: `./meow build apps && ./meow cloud store`. Devices notice within
  15 minutes.
- **Relay and pilot**: `./meow cloud relay`, `./meow cloud pilot`. Deploy the relay first when
  the protocol changes ([cloud/README.md](../cloud/README.md#protocol-json-text-frames)).
- **A device already flashed** keeps the addresses it was built with. `/data/appd/config.json`
  and `/data/net/cloud.conf` on the device override them without a rebuild.

## Local development

`npx wrangler dev` in `cloud/relay` or `cloud/store` gives a local relay or store; the
READMEs there show how to point a device on the USB network at it. The relay's tests run
against a local or a deployed instance: `node cloud/relay/test/e2e.mjs [URL]`.
