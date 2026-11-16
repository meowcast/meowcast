# Configuration

Everything that is yours and not the project's (your Cloudflare addresses, your signing key,
your device's flash dump, paths on your machine) lives in **`config.env`** at the repository
root. It is ignored by git. `./meow setup` creates it from
[`config.env.example`](../config.env.example).

- The file holds plain `KEY=VALUE` lines. It is parsed, never executed.
- The environment wins over the file: `LAB=0 ./meow build`.
- `MEOW_CONFIG=/path/to/file` uses another file, for example to keep several deployments.

| Key | Default | Meaning |
| --- | --- | --- |
| `BOARD` | `rk3228a-dongle` | Directory under `boards/` |
| `PROFILE` | the board's full profile | Board profile to build |
| `LAB` | 1 for the full profile | 0 for a build without lab machinery |
| `MEOW_STORE_URL` | empty: no store | Address of your store, with `https://` |
| `MEOW_RELAY_HOST` | empty: no cloud link | Host name of your relay |
| `MEOW_PILOT_URL` | empty | Address of your phone remote app; the TV's pairing QR code points to it |
| `MEOW_CF_RELAY_NAME`, `MEOW_CF_PILOT_NAME`, `MEOW_CF_STORE_NAME` | `meowcast-relay`, `-pilot`, `-store` | Worker names for `./meow cloud deploy` |
| `MEOW_STORE_KEY` | `cloud/store/keys/store.key` | Private key that signs your apps and assets. Created on the first build |
| `MEOW_STORE_NAME` | `meow store` | The name devices show for your store |
| `MEOW_BUILDER_IMAGE` | `meowcast-builder` | Docker image name (tags `:base` and `:gpu`) |
| `MEOW_BUILD_VOLUME` | `meowcast-build` | Docker volume with the build trees |
| `MEOW_LLVM` | Homebrew LLVM, then `PATH` | Directory with clang, ld.lld and llvm-objcopy for a board's bare-metal tools |
| `MEOW_STOCK_DUMP`, `MEOW_STOCK_SHA256` | `boards/rk3228a-dongle/stock/nor-stock.bin` and its `.sha256` | Your unit's stock flash dump (`rk3228a-dongle`) |

Cloudflare credentials are not in this file. `wrangler` keeps its own login
(`npx wrangler login`); in CI, pass `CLOUDFLARE_API_TOKEN` and `CLOUDFLARE_ACCOUNT_ID` as
environment secrets.

## What must stay private

| Thing | Where | Why |
| --- | --- | --- |
| Store signing key | `MEOW_STORE_KEY` (`*.key` is ignored by git) | Whoever has it can publish apps that every device built with its public half will install. Back it up: if you lose it, those devices cannot get updates from you |
| Stock flash dump | `boards/*/stock/` (ignored) | Vendor firmware you may not redistribute, and the only way back to it |
| `config.env` | repository root (ignored) | Names your accounts and local paths |
| Device identity and pairing tokens | on the device, under `/data` | Never leave the device except as described in [cloud/README.md](../cloud/README.md#trust-and-secrets) |

Before you publish a fork, `git grep -n -i -E 'workers\.dev|\.key|token'` should show only
documentation and code, never one of your own values.
