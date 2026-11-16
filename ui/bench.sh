#!/bin/sh
# bench.sh [MODE] [SECS]: idle (expect 0 fps, GPU 200 MHz), continuous auto-navigation
# (UI_DEMO_MS=170 keeps an animation running) with partial redraw, then with full redraw,
# then partial with GPU timing (glFinish). anim_ms = frame interval while animating.
m=${1:-1080p60}; s=${2:-12}
export UI_INTRO=${UI_INTRO:-0}  # measure the home screen, not the boot intro
echo "BENCH_BEGIN $m avail_kb=$(awk '/MemAvailable/{print $2}' /proc/meminfo)"
echo "BENCH idle"; UI_SECONDS=5 /opt/ui/run.sh "$m" 2>&1
echo "BENCH demo"; UI_DEMO=1 UI_DEMO_MS=${DEMO_MS:-170} UI_SECONDS=$s /opt/ui/run.sh "$m" 2>&1
echo "BENCH demo-full"; UI_DEMO=1 UI_DEMO_MS=${DEMO_MS:-170} UI_FULL_REDRAW=1 UI_SECONDS=$s /opt/ui/run.sh "$m" 2>&1
echo "BENCH demo-profile"; UI_DEMO=1 UI_DEMO_MS=${DEMO_MS:-170} UI_PROFILE=1 UI_SECONDS=$s /opt/ui/run.sh "$m" 2>&1
echo "BENCH_END $m avail_kb=$(awk '/MemAvailable/{print $2}' /proc/meminfo)"
