#!/bin/sh
# Deploy your own cloud side to Cloudflare Workers: the relay, the pilot (phone remote) app
# and the store. Names and URLs come from config.env at the repository root; what a first
# deployment learns (your workers.dev addresses) is written back there, and the next firmware
# build picks it up.
#
#   sh cloud/deploy.sh [all|relay|pilot|store] [--dry-run]      (or: ./meow cloud ...)
#
# Needs Node.js and a Cloudflare account: `npx wrangler login` once (in cloud/relay), or
# CLOUDFLARE_API_TOKEN and CLOUDFLARE_ACCOUNT_ID in the environment. See ../docs/CLOUD.md.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
repo=$(dirname "$here")
. "$repo/tools/config.sh"
what=all; dry=
for a in "$@"; do
    case "$a" in
        all|relay|pilot|store) what=$a ;;
        --dry-run) dry=--dry-run ;;
        *) echo "usage: deploy.sh [all|relay|pilot|store] [--dry-run]" >&2; exit 2 ;;
    esac
done
relay_name=${MEOW_CF_RELAY_NAME:-meowcast-relay}
pilot_name=${MEOW_CF_PILOT_NAME:-meowcast-pilot}
store_name=${MEOW_CF_STORE_NAME:-meowcast-store}

[ -d "$here/relay/node_modules" ] || (cd "$here/relay" && npm ci)
wrangler() { (cd "$1" && shift && npx --prefix "$here/relay" wrangler "$@"); }
# deploy DIR NAME [ARGS]: deploy a Worker and print its workers.dev URL (empty on a dry run).
deploy() {
    dir=$1; name=$2; shift 2
    log=$(mktemp)
    wrangler "$dir" deploy --name "$name" $dry "$@" 2>&1 | tee "$log" >&2
    grep -o 'https://[a-z0-9.-]*\.workers\.dev' "$log" | head -1 || true
    rm -f "$log"
}
# Learn the account's workers.dev subdomain from a deployed URL, to derive the other two.
derive() { # derive URL NAME OTHER_NAME -> https://OTHER_NAME.<subdomain>.workers.dev
    rest=${1#https://$2.}; [ "$rest" != "$1" ] && echo "https://$3.$rest"
}

do_relay() {
    origins=http://localhost:5173
    [ -n "${MEOW_PILOT_URL:-}" ] && origins="${MEOW_PILOT_URL%/},$origins"
    url=$(deploy "$here/relay" "$relay_name" --var "PILOT_ORIGINS:$origins")
    [ -n "$url" ] || return 0
    [ -n "${MEOW_RELAY_HOST:-}" ] || config_set MEOW_RELAY_HOST "${url#https://}"
    if [ -z "${MEOW_PILOT_URL:-}" ]; then
        # First deployment: now that the address of this account is known, allow the pilot
        # app's origin and deploy once more.
        pilot=$(derive "$url" "$relay_name" "$pilot_name") || true
        if [ -n "${pilot:-}" ]; then
            config_set MEOW_PILOT_URL "$pilot"
            deploy "$here/relay" "$relay_name" --var "PILOT_ORIGINS:$pilot,$origins" >/dev/null
        fi
    fi
    echo "relay: $url"
}

do_pilot() {
    [ -n "${MEOW_RELAY_HOST:-}" ] || { echo 'pilot: MEOW_RELAY_HOST is not set (deploy the relay first)' >&2; exit 2; }
    [ -d "$here/pilot/node_modules" ] || (cd "$here/pilot" && npm ci)
    (cd "$here/pilot" && VITE_CLOUD=1 VITE_RELAY="https://$MEOW_RELAY_HOST" npx vite build --outDir dist-cloud)
    url=$(deploy "$here/pilot" "$pilot_name")
    [ -n "$url" ] || return 0
    [ -n "${MEOW_PILOT_URL:-}" ] || config_set MEOW_PILOT_URL "$url"
    echo "pilot: $url"
}

do_store() {
    www=$here/store/www
    if [ ! -f "$www/index.json" ] || [ ! -f "$www/assets.json" ]; then
        echo 'store: www/ is not built yet (./meow build apps packs and signs it); skipped' >&2
        [ "$what" = store ] && exit 2
        return 0
    fi
    # Every file listed in assets.json and every app payload is checked against its SHA-256
    # before uploading (the signatures were checked by `appd verify` in build-apps.sh).
    python3 -I - "$www" <<'PY'
import hashlib, json, pathlib, sys
www = pathlib.Path(sys.argv[1])
for name, e in json.loads((www / 'assets.json').read_text())['assets'].items():
    d = (www / 'assets' / name).read_bytes()
    assert len(d) == e['size'] and hashlib.sha256(d).hexdigest() == e['sha256'], f'asset {name} does not match assets.json'
for a in json.loads((www / 'index.json').read_text())['apps']:
    m = json.loads((www / a['manifest']).read_text())
    d = (www / a['payload']).read_bytes()
    assert len(d) == m['payload']['size'] and hashlib.sha256(d).hexdigest() == m['payload']['sha256'], f"{a['id']}: payload"
print('store tree OK')
PY
    url=$(deploy "$here/store" "$store_name")
    [ -n "$url" ] || return 0
    [ -n "${MEOW_STORE_URL:-}" ] || config_set MEOW_STORE_URL "$url/"
    live=${MEOW_STORE_URL%/}
    # After it, the live assets.json must be byte-identical to the local one.
    curl -sSf "$live/assets.json" | cmp - "$www/assets.json" && echo "store: $live (assets.json matches)"
}

case "$what" in
    relay) do_relay ;;
    pilot) do_pilot ;;
    store) do_store ;;
    all) do_relay; do_pilot; do_store
         echo; echo "Rebuild the firmware (./meow build) so it uses: store=${MEOW_STORE_URL:-none} relay=${MEOW_RELAY_HOST:-none}" ;;
esac
