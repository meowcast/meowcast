#!/bin/sh
# scrollbench.sh [SECS] [REPS]: A/B frame pacing of the home screen's wallpaper modes, run
# alternately so heat affects every variant alike. Prints one UI_BENCH_SUMMARY per run.
#   vert-*: Down/Up every 800 ms (300 ms scroll + wallpaper depth dissolve)
#   horiz-*: the default row tour every 170 ms (continuous animation)
#   gpu-after     wallpaper drawn by the GPU, dissolve after the scroll (the old default)
#   gpu-during    wallpaper drawn by the GPU, dissolve during the scroll
#   planes        wallpaper on the VOP primary plane, dissolve during the scroll
#   load-*        planes, with 4 CPU-bound busy loops running (UI at nice -10, or -nice0)
s=${1:-16}; reps=${2:-2}
# ONLY=load: just the load variants (kernel A/B); PROFILE=0 skips the profile/GPU sections.
export UI_INTRO=0 UI_STATS=1
run() { # name env...
    n=$1; shift
    echo "BENCH_RUN $n temp_mc=$(cat /sys/class/thermal/thermal_zone0/temp)"
    env "$@" UI_SECONDS=$s /opt/ui/run.sh 2>&1 | grep -E "UI_BENCH_SUMMARY|UI_ERROR|UI_PLANES off"
}
i=0
while [ $i -lt $reps ]; do
  if [ "${ONLY:-}" != load ]; then
    run vert-gpu-after   UI_DEMO=1 UI_DEMO_KEYS=du UI_DEMO_MS=800 UI_PLANES=0 UI_SCROLL_BLUR=0
    run vert-gpu-during  UI_DEMO=1 UI_DEMO_KEYS=du UI_DEMO_MS=800 UI_PLANES=0 UI_SCROLL_BLUR=1
    run vert-planes      UI_DEMO=1 UI_DEMO_KEYS=du UI_DEMO_MS=800 UI_PLANES=1 UI_SCROLL_BLUR=1
    run horiz-gpu        UI_DEMO=1 UI_DEMO_MS=170 UI_PLANES=0
    run horiz-planes     UI_DEMO=1 UI_DEMO_MS=170 UI_PLANES=1
  fi
    # Same with 4 CPU-bound busy loops (nice 0) competing: scheduling latency matters here.
    for c in 1 2 3 4; do (while :; do :; done) & done
    run load-vert-planes-nice0 UI_NICE=0 UI_DEMO=1 UI_DEMO_KEYS=du UI_DEMO_MS=800 UI_PLANES=1 UI_SCROLL_BLUR=1
    run load-vert-planes  UI_DEMO=1 UI_DEMO_KEYS=du UI_DEMO_MS=800 UI_PLANES=1 UI_SCROLL_BLUR=1
    run load-horiz-planes-nice0 UI_NICE=0 UI_DEMO=1 UI_DEMO_MS=170 UI_PLANES=1
    run load-horiz-planes UI_DEMO=1 UI_DEMO_MS=170 UI_PLANES=1
    kill $(jobs -p) 2>/dev/null; wait 2>/dev/null
    i=$((i + 1))
done
[ "${PROFILE:-1}" = 0 ] && { echo "BENCH_DONE temp_mc=$(cat /sys/class/thermal/thermal_zone0/temp)"; exit 0; }
echo "BENCH_PROFILE (glFinish per frame: GPU ms, serialises CPU and GPU)"
run prof-vert-gpu-after  UI_PROFILE=1 UI_DEMO=1 UI_DEMO_KEYS=du UI_DEMO_MS=800 UI_PLANES=0 UI_SCROLL_BLUR=0
run prof-vert-gpu-during UI_PROFILE=1 UI_DEMO=1 UI_DEMO_KEYS=du UI_DEMO_MS=800 UI_PLANES=0 UI_SCROLL_BLUR=1
run prof-vert-planes     UI_PROFILE=1 UI_DEMO=1 UI_DEMO_KEYS=du UI_DEMO_MS=800 UI_PLANES=1 UI_SCROLL_BLUR=1
echo "BENCH_GPU (static frames, glFinish-timed, boosted clocks)"
UI_GPU_BENCH=1 UI_PLANES=1 UI_SECONDS=6 /opt/ui/run.sh 2>&1 | grep UI_GPU_BENCH
echo "BENCH_DONE temp_mc=$(cat /sys/class/thermal/thermal_zone0/temp)"
