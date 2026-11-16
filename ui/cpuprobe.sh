#!/bin/sh
# cpuprobe.sh [MODE] [SECS]: run the UI demo and report system CPU busy %, the UI's own
# CPU %, and CPU/GPU clocks over the middle of the run (CPU-bound vs GPU-bound check).
m=${1:-1080p60}; s=${2:-8}
UI_DEMO=1 UI_SECONDS=$((s + 5)) /opt/ui/run.sh "$m" > /tmp/cpuprobe-ui.log 2>&1 &
sleep 3
pid=$(pidof miracast-ui)
set -- $(head -n 1 /proc/stat); b0=$(($2+$3+$4+$7+$8)); t0=$(($2+$3+$4+$5+$6+$7+$8))
p0=$(awk '{print $14+$15}' /proc/$pid/stat)
sleep "$s"
set -- $(head -n 1 /proc/stat); b1=$(($2+$3+$4+$7+$8)); t1=$(($2+$3+$4+$5+$6+$7+$8))
p1=$(awk '{print $14+$15}' /proc/$pid/stat)
echo "CPUPROBE mode=$m sys_busy_pct=$(( 100 * (b1-b0) / (t1-t0) )) ui_pct=$(( 100 * (p1-p0) / (t1-t0) )) cpu_khz=$(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq) gpu_hz=$(cat /sys/class/devfreq/20000000.gpu/cur_freq) gpu_load=$(cat /sys/class/devfreq/20000000.gpu/load 2>/dev/null)"
wait
grep -E 'UI_STATS t=(5|8|10)|UI_EXIT' /tmp/cpuprobe-ui.log
