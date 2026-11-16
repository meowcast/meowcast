#!/bin/sh
# Frame pacing of the top bar and the Settings view: runs the UI through a scripted tour
# (top-bar focus, Settings open/close, categories, rows, pickers, switches, scrolling lists)
# and prints its UI_BENCH_SUMMARY line (late = an interval over 1.5 vsyncs).
#   usage: /opt/ui/settingsbench.sh [1080p60|4k30] [ROUNDS] [KEY_MS]
# Settings go to a scratch file (no flash writes); the boot UI session is stopped first.
OUT=${1:-1080p60}
ROUNDS=${2:-3}
KEY_US=$(( ${3:-220} * 1000 ))
[ -x /bin/ui-session ] && pidof miracast-ui >/dev/null && ui-session stop >/dev/null
killall run.sh miracast-ui 2>/dev/null
sleep 1
rm -f /tmp/bench-settings
LOG=/tmp/settingsbench.log
UI_SETTINGS=/tmp/bench-settings UI_OUTPUT=$OUT UI_INTRO=0 UI_STATS=1 UI_FONTS=/opt/ui/fonts \
    LD_LIBRARY_PATH=/opt/ui/lib /opt/ui/bin/miracast-ui >"$LOG" 2>&1 &
sleep 5
k() {
    for c in $(echo "$1" | sed 's/./& /g'); do
        printf "$c" > /run/ui-keys
        usleep $KEY_US
    done
}
i=0
while [ $i -lt "$ROUNDS" ]; do
    k wddaa                 # top bar: in, along, back
    k e                     # open Settings (Wi-Fi)
    k sssssssawwwwwww       # categories down and up
    k ssdsswwb              # Display: rows down/up, back to the categories
    k sd                    # Wallpaper: into the grid (on the wallpaper in use)
    k dddsssaaawww          # around the grid
    k dse                   # use another wallpaper: its preview, then the download (two cross-fades)
    k awe                   # and the first one again (from appd's RAM cache)
    k b                     # back to the categories
    k sd                    # Date & time: into rows
    k essssssssswwwwwwwwwb  # zone picker: scroll down and up, cancel
    k seeb                  # 24-hour switch on, off; back to categories
    k ssd                   # Developer
    k ee                    # overlay on, off
    k bb                    # close Settings
    k sssddaawww            # home rows (wallpaper depth), back to the top
    k s
    i=$((i + 1))
done
sleep 1
printf q > /run/ui-keys
sleep 2
grep -E "^UI_STALL" "$LOG" | tail -n 5
grep -E "^UI_PREFETCH_DONE|^UI_SLOW_BUILD|^UI_WARN|^UI_LAUNCH phase|^UI_BENCH_SUMMARY|^UI_EXIT" "$LOG"
