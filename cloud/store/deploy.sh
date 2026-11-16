#!/bin/sh
# Publish the store (www/) to Cloudflare: see ../deploy.sh, which does the work.
exec sh "$(dirname "$0")/../deploy.sh" store "$@"
