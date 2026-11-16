#!/bin/sh
# Launch the UI from /opt/ui. usage: run.sh [1080p60|4k30|720p60]  (extra env passes through,
# e.g. UI_DEMO=1 UI_SECONDS=20 /opt/ui/run.sh 4k30). Keys: printf d > /run/ui-keys
# The boot session (ui-session, initramfs) owns the display: stop it first.
# Exit code 3 (a new resolution or Restart in Settings) starts it again in the saved output.
[ -x /bin/ui-session ] && pidof miracast-ui >/dev/null && ui-session stop >/dev/null
export UI_OUTPUT=${1:-${UI_OUTPUT:-1080p60}}
export UI_FONTS=/opt/ui/fonts
[ -d /opt/ui/lib ] && export LD_LIBRARY_PATH=/opt/ui/lib
while :; do
    /opt/ui/bin/miracast-ui
    rc=$?
    [ $rc -eq 3 ] || exit $rc
    m=$(sed -n 's/^output=//p' "${UI_SETTINGS:-/data/ui/settings}" 2>/dev/null | head -n 1)
    [ -n "$m" ] && export UI_OUTPUT=$m
    export UI_INTRO=entrance UI_TV_SYNC_MS=0
done
