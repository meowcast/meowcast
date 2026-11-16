# Board description, sourced by os/build.sh (POSIX sh). See boards/README.md for every field.
# RK3228A HDMI dongle: 256 MiB DDR3, 16 MiB SPI NOR, Mali-400, one USB OTG port, one button.
BOARD_DESCRIPTION='RK3228A Miracast dongle (256 MiB DDR3, 16 MiB SPI NOR)'

# Kernel and userspace toolchain (the builder image provides it).
BOARD_ARCH=arm
BOARD_CROSS_COMPILE=arm-linux-gnueabihf-
# For the small static C daemons built by os/build.sh.
BOARD_CFLAGS='-mcpu=cortex-a7 -mthumb'

# Device trees: everything in dts/ is copied to arch/$BOARD_ARCH/boot/dts/$BOARD_DTS_DIR.
BOARD_DTS_DIR=rockchip
# The name os/build.sh gives the built DTB in os/output (the boot and flash tools read it).
BOARD_DTB_OUT=rk3228a-miracast-ram.dtb
# Profile used when PROFILE is not set, and the profiles this board has.
BOARD_DEFAULT_PROFILE=perf
BOARD_PROFILES='perf graphics hdmi-direct display usb'
# The profile with everything a product needs (SMP, DVFS, thermal, flash /data, Wi-Fi, media).
BOARD_FULL_PROFILE=perf

# Board-only diagnostic scripts from rootfs/, installed into /bin.
BOARD_TOOLS='perf-report clk-set scdc-status hdmi-phy hdmi4k60-diag gpu-bench gpu-soak vlogic-ab clk-gate-probe regab idletemp clkab ddrtest vlogic-floor clk-gate-unused'
# Extra ones for LAB=1 images.
BOARD_LAB_TOOLS='reg lab-status lab-reboot lab-diff'

# board_dts PROFILE LAB: print the DTS base name for a profile.
board_dts() {
    case "$1" in
        usb) echo rk3228a-miracast-usb ;;
        graphics) echo rk3228a-miracast-graphics ;;
        hdmi-direct) echo rk3228a-miracast-hdmi-direct ;;
        display) echo rk3228a-miracast-ram ;;
        perf) if [ "$2" = 1 ]; then echo rk3228a-miracast-perf-lab; else echo rk3228a-miracast-perf; fi ;;
        *) return 2 ;;
    esac
}
# Included after the profile's DTS when MEDIA=1 (hardware video decoder, HDMI audio).
BOARD_MEDIA_DTSI=rk3228a-miracast-media.dtsi
