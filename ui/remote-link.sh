#!/bin/sh
# remote-link.sh: forward keys from the Mac's phone-remote relay (cloud/pilot/server.mjs)
# into the UI's key FIFO. The dongle connects OUT to the Mac over the USB link, so it opens no
# listening port. Reconnects forever; start once per boot:
#   setsid /opt/ui/remote-link.sh >/dev/null 2>&1 </dev/null &
. /run/net.env 2>/dev/null
host=${REMOTE_HOST:-${DEV_HOST:-${GATEWAY:-192.168.2.1}}}
port=${REMOTE_PORT:-8002}
fifo=${UI_KEYS:-/run/ui-keys}
while :; do
    # Only when the UI has created its FIFO (a redirect would otherwise make a plain file).
    if [ -p "$fifo" ]; then
        nc "$host" "$port" > "$fifo"
    fi
    sleep 2
done
