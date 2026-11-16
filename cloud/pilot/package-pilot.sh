#!/bin/sh
# package-pilot.sh: stage the dongle's own web page for the initramfs (../../os/deploy/pilot ->
# /usr/share/pilot, served by meow-netd on port 80). Since 14 Nov that is setup/index.html only:
# the Wi-Fi setup (captive portal) and a link to the pilot app, which runs from Cloudflare
# (npm run deploy:cloud). Run on the Mac: sh package-pilot.sh
set -eu
cd "$(dirname "$0")"
out=../../os/deploy/pilot
rm -rf "$out"
mkdir -p "$out"
cp setup/index.html "$out/index.html"
gzip -9 -n -c "$out/index.html" > "$out/index.html.gz"
(cd "$out" && find . -type f ! -name SHA256SUMS | sort | xargs shasum -a 256 > SHA256SUMS)
ls -l "$out"
