#!/bin/sh
set -eu
# Run inside the builder with the repository mounted (any path) and a Linux Docker volume at
# /build: `./meow build` does that. Keeping kernel metadata in the volume is much faster.
# BOARD picks boards/<name> (board.sh, dts/, kernel/, rootfs/): see ../boards/README.md.
cd "$(dirname "$0")"
board=../boards/${BOARD:-rk3228a-dongle}
[ -f "$board/board.sh" ] || { echo "no board description: $board/board.sh"; exit 2; }
. "$board/board.sh"
export LC_ALL=C TZ=UTC SOURCE_DATE_EPOCH=1794355200
export KBUILD_BUILD_TIMESTAMP='2026-11-11 00:00:00 UTC'
export KBUILD_BUILD_USER=ramboot KBUILD_BUILD_HOST=miracast KBUILD_BUILD_VERSION=1
export ARCH=$BOARD_ARCH CROSS_COMPILE=$BOARD_CROSS_COMPILE
mkdir -p /build output
echo 'b11b6df8295e8e991ff040b71aaa4994a8b55caa23b6b092907f195b5840fd1f  sources/linux-7.3.4.tar.xz' | sha256sum -c -
echo '3311dff32e746499f4df0d5df04d7eb396382d7e108bb9250e7b519b837043a4  sources/busybox-1.37.0.tar.bz2' | sha256sum -c -
[ -d /build/linux-7.3.4 ] || tar -xJf sources/linux-7.3.4.tar.xz -C /build
[ -d /build/busybox-1.37.0 ] || tar -xjf sources/busybox-1.37.0.tar.bz2 -C /build
jobs=${JOBS:-8}
profile=${PROFILE:-$BOARD_DEFAULT_PROFILE}
full=$BOARD_FULL_PROFILE
board_dts "$profile" 0 >/dev/null || { echo "board ${BOARD:-rk3228a-dongle} has no profile '$profile' (it has: $BOARD_PROFILES)"; exit 2; }
# LAB=1: lab/debug machinery (MaskROM auto-recovery, labreg, writable debugfs,
# sysrq, lab scripts). Default on for the perf profile, off otherwise; only
# valid with PROFILE=perf. See LAB-RECOVERY.md for the code-path inventory.
if [ "$profile" = "$full" ]; then lab=${LAB:-1}; else lab=0; fi
[ "${LAB:-0}" = 1 ] && [ "$profile" != "$full" ] && { echo 'LAB=1 needs PROFILE=perf'; exit 2; }
bb=/build/busybox-1.37.0
make -C "$bb" CROSS_COMPILE="$CROSS_COMPILE" allnoconfig
python3 - "$bb/.config" busybox.config <<'PY'
from pathlib import Path
import re,sys
p=Path(sys.argv[1]); text=p.read_text()
for line in Path(sys.argv[2]).read_text().splitlines():
    m=re.match(r'(CONFIG_\w+)=',line)
    if m:
        key=m[1]
        text,n=re.subn(r'^(?:'+key+r'=.*|# '+key+r' is not set)$',lambda _:line,text,flags=re.M)
        if not n:
            text+=line+'\n'
p.write_text(text)
PY
yes '' | make -C "$bb" CROSS_COMPILE="$CROSS_COMPILE" oldconfig
make -C "$bb" CROSS_COMPILE="$CROSS_COMPILE" -j"$jobs"
# This is solely our generated staging tree in the dedicated Docker volume.
rm -rf /build/rootfs
mkdir -p /build/rootfs
make -C "$bb" CROSS_COMPILE="$CROSS_COMPILE" CONFIG_PREFIX=/build/rootfs install
mkdir -p /build/rootfs/dev /build/rootfs/proc /build/rootfs/sys /build/rootfs/run /build/rootfs/tmp
cp rootfs/init /build/rootfs/init
cp rootfs/bringup-report /build/rootfs/bin/bringup-report
for f in usb-gadget net-up net-status fetch cpuburn-sh cpustress; do cp rootfs/$f /build/rootfs/bin/; chmod 755 /build/rootfs/bin/$f; done
for f in $BOARD_TOOLS; do cp "$board/rootfs/$f" /build/rootfs/bin/; chmod 755 /build/rootfs/bin/$f; done
if [ "$lab" = 1 ]; then
    for f in $BOARD_LAB_TOOLS; do cp "$board/rootfs/$f" /build/rootfs/bin/; chmod 755 /build/rootfs/bin/$f; done
fi
chmod 755 /build/rootfs/init /build/rootfs/bin/bringup-report
mkdir -p /build/rootfs/etc /build/rootfs/opt /build/rootfs/usr/share/udhcpc
cp rootfs/udhcpc.script /build/rootfs/usr/share/udhcpc/default.script
chmod 755 /build/rootfs/usr/share/udhcpc/default.script
printf '127.0.0.1 localhost\n127.0.1.1 rk3228\n' > /build/rootfs/etc/hosts
printf 'hosts: files dns\n' > /build/rootfs/etc/nsswitch.conf
# UI=1 (default with GPU_TEST=1): bake the TV UI into the initramfs; init starts it at boot.
# Build it first with ../ui/build-ui.sh, which writes deploy/ui (binary + fonts).
ui=${UI:-${GPU_TEST:-0}}
if [ "$ui" = 1 ]; then
    [ "${GPU_TEST:-0}" = 1 ] || { echo 'UI=1 needs GPU_TEST=1'; exit 2; }
    [ -f deploy/ui/bin/miracast-ui ] || { echo 'UI=1: run ../ui/build-ui.sh first (or UI=0)'; exit 2; }
    (cd deploy/ui && sha256sum -c --quiet SHA256SUMS)
fi
# APPD=1 (default when ../apps/build-apps.sh has written deploy/appd): the app manager
# (appd + appctl + appd-session) and the trusted store key(s). Needs the glibc runtime that
# package-gpu.py copies, so GPU_TEST=1.
appd=${APPD:-0}
[ -z "${APPD:-}" ] && [ -f deploy/appd/bin/appd ] && [ "${GPU_TEST:-0}" = 1 ] && appd=1
if [ "$appd" = 1 ]; then
    [ "${GPU_TEST:-0}" = 1 ] || { echo 'APPD=1 needs GPU_TEST=1'; exit 2; }
    (cd deploy/appd && sha256sum -c --quiet SHA256SUMS)
fi
# WIFI=1 (default for perf when ../wifi/build-userspace.sh has written deploy/wifi): dual-role
# USB + cfg80211 + the RTL8733BU driver in the kernel, wpa_supplicant/dropbear in the rootfs.
wifi=${WIFI:-0}
[ -z "${WIFI:-}" ] && [ "$profile" = "$full" ] && [ -f deploy/wifi/bin/wpa_supplicant ] && [ -d ../wifi/vendor/rtl8733bu ] && wifi=1
if [ "$wifi" = 1 ]; then
    [ "$profile" = "$full" ] || { echo 'WIFI=1 needs PROFILE=perf'; exit 2; }
    [ -d ../wifi/vendor/rtl8733bu ] || { echo 'WIFI=1: no ../wifi/vendor/rtl8733bu (see ../wifi/README.md)'; exit 2; }
    (cd deploy/wifi && sha256sum -c --quiet SHA256SUMS)
fi
if [ "${GPU_TEST:-0}" = 1 ]; then
    UI=$ui APPD=$appd sh ./package-gpu.sh
fi
if [ "$wifi" = 1 ]; then
    mkdir -p /build/rootfs/usr/sbin
    for f in wpa_supplicant wpa_cli dropbear dropbearkey meow-tls; do
        cp deploy/wifi/bin/$f /build/rootfs/usr/sbin/$f
        chmod 755 /build/rootfs/usr/sbin/$f
    done
    # The cloud link (meow-netd cloud.rs): relay host and pilot app URL, from config.env
    # (../cloud/README.md). Without MEOW_RELAY_HOST the firmware has no cloud link.
    mkdir -p /build/rootfs/etc/meow
    printf 'relay=%s\napp=%s\n' "${MEOW_RELAY_HOST-}" "${MEOW_PILOT_URL-}" > /build/rootfs/etc/meow/cloud.conf
    if [ -f "$board/rootfs/usb-role" ]; then
        cp "$board/rootfs/usb-role" /build/rootfs/bin/usb-role
        chmod 755 /build/rootfs/bin/usb-role
    fi
fi
if [ "$appd" = 1 ]; then
    mkdir -p /build/rootfs/etc/appd/keys
    cp deploy/appd/keys/*.pub /build/rootfs/etc/appd/keys/
    chmod 644 /build/rootfs/etc/appd/keys/*.pub
    # The store (apps + assets: wallpaper, full fonts), a static site (../cloud/store), from
    # config.env. HTTPS needs meow-tls (WIFI=1 builds); MEOW_STORE_URL=http://... for a
    # `wrangler dev` on the build host; without it the firmware has no store.
    printf '%s\n' "${MEOW_STORE_URL-}" > /build/rootfs/etc/appd/store.conf
    ln -sf appd /build/rootfs/usr/bin/appctl
    cp rootfs/appd-session /build/rootfs/bin/appd-session
    chmod 755 /build/rootfs/bin/appd-session
    # The network daemon is the same binary (multi-call): pilot server, Wi-Fi, debug bridge.
    ln -sf appd /build/rootfs/usr/bin/meow-netd
    cp rootfs/netd-session /build/rootfs/bin/netd-session
    chmod 755 /build/rootfs/bin/netd-session
    # The pilot web app it serves (../cloud/pilot/package-pilot.sh writes deploy/pilot).
    if [ -f deploy/pilot/index.html ]; then
        mkdir -p /build/rootfs/usr/share/pilot
        cp -r deploy/pilot/. /build/rootfs/usr/share/pilot/
    fi
    # root for the debug bridge (dropbear needs a passwd entry; no password: keys only).
    printf 'root:x:0:0:root:/root:/bin/sh\n' > /build/rootfs/etc/passwd
    printf 'root:x:0:\n' > /build/rootfs/etc/group
    mkdir -p /build/rootfs/root
    chmod 700 /build/rootfs/root
fi
if [ "$ui" = 1 ]; then
    mkdir -p /build/rootfs/usr/share/fonts/noto
    cp deploy/ui/fonts/*.ttf /build/rootfs/usr/share/fonts/noto/
    cp rootfs/ui-session /build/rootfs/bin/ui-session
    chmod 755 /build/rootfs/bin/ui-session
fi
# Thermal supervisor (thermald.c): static, so it needs nothing from the rootfs but /bin/sh for
# rootfs/thermal-actions. Perf profile only (it needs the TSADC, cpufreq and devfreq).
if [ "$profile" = "$full" ] && [ "${THERMALD:-1}" = 1 ]; then
    mkdir -p /build/rootfs/usr/bin output/thermal
    ${CROSS_COMPILE}gcc -Os -static $BOARD_CFLAGS -Wall -Wextra -Werror -ffunction-sections -fdata-sections \
        -Wl,--gc-sections thermald.c -o output/thermal/meow-thermald
    ${CROSS_COMPILE}strip output/thermal/meow-thermald
    cp output/thermal/meow-thermald /build/rootfs/usr/bin/meow-thermald
    for f in thermald-session thermal-actions; do cp rootfs/$f /build/rootfs/bin/; chmod 755 /build/rootfs/bin/$f; done
    chmod 755 /build/rootfs/usr/bin/meow-thermald
fi
# The hardware button (buttond.c): pairing screen on a short press, factory reset on a long
# hold let go in its window (rootfs/factory-reset). Static like the thermal daemon. Its timing
# rules are tested here, natively, before anything is packed.
if [ "$profile" = "$full" ] && [ "${BUTTOND:-1}" = 1 ]; then
    mkdir -p /build/rootfs/usr/bin output/button
    gcc -O1 -Wall -Wextra -Werror buttond.c -o /tmp/meow-buttond-host
    /tmp/meow-buttond-host selftest
    ${CROSS_COMPILE}gcc -Os -static $BOARD_CFLAGS -Wall -Wextra -Werror -ffunction-sections -fdata-sections \
        -Wl,--gc-sections buttond.c -o output/button/meow-buttond
    ${CROSS_COMPILE}strip output/button/meow-buttond
    cp output/button/meow-buttond /build/rootfs/usr/bin/meow-buttond
    for f in buttond-session factory-reset; do cp rootfs/$f /build/rootfs/bin/; chmod 755 /build/rootfs/bin/$f; done
    chmod 755 /build/rootfs/usr/bin/meow-buttond
fi
# gen_init_cpio handles console device nodes without privileged mknod.
linux=/build/linux-7.3.4
mkdir -p /build/linux-out
cp "$board/kernel/kernel.config" /build/linux-out/.config
if [ "${GPU_TEST:-0}" = 1 ]; then
    echo 'CONFIG_NEON=y' >> /build/linux-out/.config
fi
# MEDIA=1 (default for perf): rkvdec hardware video decoder + HDMI audio, for video apps
# (apps with the "media" permission, apps/youtube). Adds kernel-media.config and
# board/rk3228a-miracast-media.dtsi; perf profile only. MEDIA=0 builds without them.
media=${MEDIA:-0}
[ -z "${MEDIA:-}" ] && [ "$profile" = "$full" ] && media=1
if [ "$media" = 1 ]; then
    [ "$profile" = "$full" ] || { echo 'MEDIA=1 needs PROFILE=perf'; exit 2; }
    cat "$board/kernel/kernel-media.config" >> /build/linux-out/.config
fi
# Source adjustments are re-applied idempotently because the kernel tree persists in the
# Docker volume between builds. First the ones every board gets, then the board's own
# (kernel/patch.sh, further down).
# UI latency under CPU load (13 Nov): the GPU job hand-offs (Lima's drm_sched submit
# work: run GP job, then the PP job when GP is done) and the nonblocking atomic commit
# tail (waits for the GPU fence, programs the VOP before vblank) ran in normal-priority
# kworkers, which queue behind busy threads at the same nice level (scrolls measured
# 14-19 % late frames with 4 busy loops). Lima gets a WQ_HIGHPRI ordered submit queue
# per pipe; nonblocking commits go to system_highpri_wq.
python3 - "$linux" <<'PY'
import sys
from pathlib import Path
root = Path(sys.argv[1])
p = root/'drivers/gpu/drm/lima/lima_sched.c'
s = p.read_text()
if 'lima_submit_wq' not in s:
    s = s.replace("""	const struct drm_sched_init_args args = {
		.ops = &lima_sched_ops,""", """	/* lima_submit_wq: high-priority job hand-off (miracast build.sh) */
	struct workqueue_struct *wq = alloc_ordered_workqueue("lima-%s", WQ_HIGHPRI | WQ_MEM_RECLAIM, name);
	const struct drm_sched_init_args args = {
		.ops = &lima_sched_ops,
		.submit_wq = wq,""", 1)
    s = s.replace("""void lima_sched_pipe_fini(struct lima_sched_pipe *pipe)
{
	drm_sched_fini(&pipe->base);
}""", """void lima_sched_pipe_fini(struct lima_sched_pipe *pipe)
{
	struct workqueue_struct *wq = pipe->base.own_submit_wq ? NULL : pipe->base.submit_wq;

	drm_sched_fini(&pipe->base);
	if (wq)
		destroy_workqueue(wq);
}""", 1)
    assert s.count('lima_submit_wq') == 1 and 'destroy_workqueue(wq)' in s
    p.write_text(s)
p = root/'drivers/gpu/drm/drm_atomic_helper.c'
s = p.read_text()
old = """	if (nonblock)
		queue_work(system_dfl_wq, &state->commit_work);"""
if old in s:
    s = s.replace(old, """	if (nonblock) /* miracast build.sh: high priority (UI frame latency under load) */
		queue_work(system_highpri_wq, &state->commit_work);""", 1)
    p.write_text(s)
assert 'queue_work(system_highpri_wq, &state->commit_work);' in p.read_text()
PY
regmap_dbg="$linux/drivers/base/regmap/regmap-debugfs.c"
if [ "$profile" = "$full" ]; then
    # Production-relevant: SMP, hardware watchdog core, lockup detector. The board's hook adds
    # its own watchdog, SPI, thermal-sensor and PWM drivers.
    printf 'CONFIG_SMP=y\nCONFIG_NR_CPUS=4\nCONFIG_HOTPLUG_CPU=y\n# CONFIG_FORCE_NR_CPUS is not set\nCONFIG_WATCHDOG=y\nCONFIG_WATCHDOG_CORE=y\nCONFIG_WATCHDOG_HANDLE_BOOT_ENABLED=y\nCONFIG_SOFTLOCKUP_DETECTOR=y\n' >> /build/linux-out/.config
    # Production-relevant (13 Nov): app sandbox (cgroup v2 memory/pids/cpu limits, Landlock
    # LSM; namespaces and seccomp are already on) and the SPI-NOR data partition (JFFS2 on
    # /data). No MTD char/block devices: Linux reaches only the DT "data" partition.
    printf 'CONFIG_CGROUPS=y\nCONFIG_MEMCG=y\nCONFIG_CGROUP_PIDS=y\nCONFIG_CGROUP_SCHED=y\nCONFIG_FAIR_GROUP_SCHED=y\nCONFIG_CFS_BANDWIDTH=y\nCONFIG_SECURITY=y\nCONFIG_SECURITY_LANDLOCK=y\nCONFIG_LSM="landlock"\nCONFIG_SPI=y\nCONFIG_SPI_MASTER=y\nCONFIG_MTD=y\nCONFIG_MTD_OF_PARTS=y\nCONFIG_MTD_SPI_NOR=y\n# CONFIG_MTD_SPI_NOR_USE_4K_SECTORS is not set\n# CONFIG_MTD_PARTITIONED_MASTER is not set\n# CONFIG_MTD_CHAR is not set\n# CONFIG_MTD_BLOCK is not set\nCONFIG_JFFS2_FS=y\nCONFIG_JFFS2_SUMMARY=y\n' >> /build/linux-out/.config
    # Compressed swap in RAM (rootfs/init sets it up): cold tmpfs pages (the rootfs itself) and idle
    # daemons shrink ~3x under memory pressure. lz4: the cheapest page-in.
    printf 'CONFIG_ZRAM=y\nCONFIG_ZRAM_BACKEND_LZ4=y\nCONFIG_ZRAM_DEF_COMP_LZ4=y\nCONFIG_ZSMALLOC=y\n' >> /build/linux-out/.config
    printf 'CONFIG_THERMAL=y\nCONFIG_THERMAL_OF=y\nCONFIG_I2C_CHARDEV=y\nCONFIG_PWM=y\nCONFIG_REGULATOR_PWM=y\nCONFIG_PM_DEVFREQ=y\nCONFIG_DEVFREQ_GOV_PERFORMANCE=y\nCONFIG_DEVFREQ_GOV_USERSPACE=y\nCONFIG_DEVFREQ_THERMAL=y\nCONFIG_CPU_FREQ=y\nCONFIG_CPUFREQ_DT=y\nCONFIG_CPU_FREQ_STAT=y\nCONFIG_CPU_FREQ_GOV_ONDEMAND=y\nCONFIG_CPU_FREQ_DEFAULT_GOV_ONDEMAND=y\nCONFIG_CPU_FREQ_GOV_PERFORMANCE=y\nCONFIG_CPU_FREQ_GOV_USERSPACE=y\nCONFIG_CPU_THERMAL=y\n' >> /build/linux-out/.config
    # KLAT=1 (experiment, 13 Nov): low-latency scheduling for the UI (full preemption,
    # high-resolution timers, tickless idle, 250 Hz tick) instead of voluntary / 100 Hz periodic.
    if [ "${KLAT:-0}" = 1 ]; then
        printf '# CONFIG_PREEMPT_VOLUNTARY is not set\nCONFIG_PREEMPT=y\nCONFIG_HIGH_RES_TIMERS=y\nCONFIG_NO_HZ_IDLE=y\n# CONFIG_HZ_PERIODIC is not set\n# CONFIG_HZ_100 is not set\nCONFIG_HZ_250=y\n' >> /build/linux-out/.config
    fi
    if [ "$wifi" = 1 ]; then
        sh ../wifi/kernel-wifi.sh "$linux" /build/linux-out/.config
    fi
fi
if [ "$lab" = 1 ]; then
    # LAB-ONLY. Writable clk/regmap debugfs; sysrq (lab-reboot uses sysrq b).
    sed -i 's/^#undef CLOCK_ALLOW_WRITE_DEBUGFS$/#define CLOCK_ALLOW_WRITE_DEBUGFS/' "$linux/drivers/clk/clk.c"
    sed -i 's/^#undef REGMAP_ALLOW_WRITE_DEBUGFS$/#define REGMAP_ALLOW_WRITE_DEBUGFS/' "$regmap_dbg"
    printf 'CONFIG_MAGIC_SYSRQ=y\n' >> /build/linux-out/.config
else
    sed -i 's/^#define CLOCK_ALLOW_WRITE_DEBUGFS$/#undef CLOCK_ALLOW_WRITE_DEBUGFS/' "$linux/drivers/clk/clk.c"
    sed -i 's/^#define REGMAP_ALLOW_WRITE_DEBUGFS$/#undef REGMAP_ALLOW_WRITE_DEBUGFS/' "$regmap_dbg"
fi
# The board's own kernel source patches, extra drivers and config lines.
if [ -f "$board/kernel/patch.sh" ]; then
    full_flag=0; [ "$profile" = "$full" ] && full_flag=1
    BOARD_DIR=$board LINUX=$linux KCONFIG=/build/linux-out/.config PROFILE=$profile FULL=$full_flag LAB=$lab \
        sh "$board/kernel/patch.sh"
fi
board_name=$(board_dts "$profile" "$lab")
[ "$profile" = usb ] && sed -i 's/CONFIG_U_SERIAL_CONSOLE=y/# CONFIG_U_SERIAL_CONSOLE is not set/' /build/linux-out/.config
make -C "$linux" O=/build/linux-out olddefconfig
mkdir -p /build/linux-out/usr
gcc -O2 "$linux/usr/gen_init_cpio.c" -o /build/linux-out/usr/gen_init_cpio
printf 'nod /dev/console 600 0 0 c 5 1\n' > /build/console.list
(cd /build/linux-out && "$linux/usr/gen_initramfs.sh" -d "$KBUILD_BUILD_TIMESTAMP" -o /build/rootfs.cpio /build/rootfs /build/console.list)
# Compress it here rather than in the kernel build (kernel.config has the numbers): the
# userspace is Thumb-2 code, so the ARM-Thumb BCJ filter helps, and one dictionary spans the
# whole archive. CRC32 is the only check the kernel's unpacker has. Single-threaded and with
# fixed options, so the output is reproducible.
cpio_mib=$(( ($(stat -c %s /build/rootfs.cpio) + 1048575) / 1048576 ))
xz --check=crc32 -T1 --armthumb --lzma2=preset=9e,dict=${cpio_mib}MiB,lc=1,lp=1,pb=1 -c /build/rootfs.cpio > /build/rootfs.cpio.xz
dts_dir=$linux/arch/$ARCH/boot/dts/$BOARD_DTS_DIR
cp "$board"/dts/* "$dts_dir/"
if [ "$media" = 1 ]; then
    printf '#include "%s.dts"\n#include "%s"\n' "$board_name" "$BOARD_MEDIA_DTSI" > "$dts_dir/meow-media.dts"
    board_name=meow-media
fi
make -C "$linux" O=/build/linux-out -j"$jobs" zImage "$BOARD_DTS_DIR/$board_name.dtb"
cp "/build/linux-out/arch/$ARCH/boot/zImage" output/zImage
cp "/build/linux-out/arch/$ARCH/boot/dts/$BOARD_DTS_DIR/$board_name.dtb" "output/$BOARD_DTB_OUT"
echo "${BOARD:-rk3228a-dongle}" > output/board
echo "$profile" > output/profile
printf '{"gpu_test":%s,"lab":%s,"ui":%s,"appd":%s,"wifi":%s,"media":%s}\n' "$(if [ "${GPU_TEST:-0}" = 1 ]; then echo true; else echo false; fi)" "$(if [ "$lab" = 1 ]; then echo true; else echo false; fi)" "$(if [ "$ui" = 1 ]; then echo true; else echo false; fi)" "$(if [ "$appd" = 1 ]; then echo true; else echo false; fi)" "$(if [ "$wifi" = 1 ]; then echo true; else echo false; fi)" "$(if [ "$media" = 1 ]; then echo true; else echo false; fi)" > output/features.json
cp /build/linux-out/.config output/linux.config
cp "$bb/.config" output/busybox.config
cp /build/rootfs.cpio output/
cp "$bb/busybox" output/
${CROSS_COMPILE}gcc --version > output/toolchain.txt
dpkg-query -W > output/builder-packages.txt
sha256sum output/zImage output/*.dtb output/rootfs.cpio output/busybox > output/SHA256SUMS
ls -lh output/zImage output/*.dtb output/rootfs.cpio output/busybox
echo "initramfs: $(stat -c %s /build/rootfs.cpio) bytes, $(stat -c %s /build/rootfs.cpio.xz) as xz; zImage $(stat -c %s output/zImage) bytes"
