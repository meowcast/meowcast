#!/bin/sh
# Kernel hook of this board, run by os/build.sh after the generic source patches and config
# lines, before olddefconfig. Environment: LINUX (source tree), KCONFIG (.config to append to),
# BOARD_DIR, PROFILE, FULL (1 for the full product profile), LAB (1 for lab images).
# Every change is idempotent and undone when its condition is off: the tree persists in the
# builder volume between builds.
set -eu
linux=$LINUX
board=$BOARD_DIR
# aclk_gpu is a gate; without CLK_SET_RATE_PARENT neither assigned-clock-rates
# nor Lima devfreq can change the GPU clock (aclk_gpu_pre has no DT ID).
sed -i 's/GATE(ACLK_GPU, "aclk_gpu", "aclk_gpu_pre", 0,/GATE(ACLK_GPU, "aclk_gpu", "aclk_gpu_pre", CLK_SET_RATE_PARENT,/' \
    "$linux/drivers/clk/rockchip/clk-rk3228.c"
grep -q 'GATE(ACLK_GPU, "aclk_gpu", "aclk_gpu_pre", CLK_SET_RATE_PARENT,' "$linux/drivers/clk/rockchip/clk-rk3228.c"
# The HDMI controller's registers sit behind the VIO AHB-to-APB bridge, which
# upstream does not mark critical: without clk_ignore_unused it is gated at late
# init, the dw_hdmi IRQ can no longer be cleared ("nobody cared"), HPD never
# fires and the display stays off (measured 12 Nov). Keep the bridge on.
grep -q '^	"hclk_vio_h2p",$' "$linux/drivers/clk/rockchip/clk-rk3228.c" || \
    sed -i '/^static const char \*const rk3228_critical_clocks\[\] __initconst = {$/,/^};$/ s/^\t"hclk_vop_noc",$/\t"hclk_vop_noc",\n\t"hclk_vio_h2p",\n\t"pclk_vio_h2p",/' \
    "$linux/drivers/clk/rockchip/clk-rk3228.c"
grep -q '^	"pclk_vio_h2p",$' "$linux/drivers/clk/rockchip/clk-rk3228.c"
# Upstream registers CPLL without a rate table (read-only at the bootloader's
# 500 MHz). Give it the shared RK3228 table so CPLL can feed VOP/GPU at
# 1200 MHz / n. No other profile requests a CPLL rate.
sed -i 's/RK2928_MODE_CON, 8, 8, 0, NULL),/RK2928_MODE_CON, 8, 8, 0, rk3228_pll_rates),/' \
    "$linux/drivers/clk/rockchip/clk-rk3228.c"
grep -q 'RK2928_MODE_CON, 8, 8, 0, rk3228_pll_rates),' "$linux/drivers/clk/rockchip/clk-rk3228.c"
# rkvdec keeps its four clocks running for ever (15 Nov: aclk_rkvdec 192 MHz, hclk_rkvdec,
# sclk_vdec_core and sclk_vdec_cabac 300 MHz all enabled with the decoder runtime-suspended and
# nothing playing): probe gets them already enabled and runtime PM enables them once more, so
# runtime suspend only drops the count from 2 to 1. Probe touches no decoder register; take
# the clocks without enabling them and let runtime resume/suspend switch them, as before 6.x.
rkvdec="$linux/drivers/media/platform/rockchip/rkvdec/rkvdec.c"
if [ -f "$rkvdec" ]; then
    sed -i 's/ret = devm_clk_bulk_get_all_enabled(&pdev->dev, &rkvdec->clocks);/ret = devm_clk_bulk_get_all(\&pdev->dev, \&rkvdec->clocks); \/* miracast build.sh: runtime PM owns the clocks *\//' "$rkvdec"
    grep -q 'ret = devm_clk_bulk_get_all(&pdev->dev, &rkvdec->clocks);' "$rkvdec"
fi
pmd="$linux/drivers/pmdomain/rockchip/pm-domains.c"
socmk="$linux/drivers/soc/rockchip/Makefile"
if [ "$FULL" = 1 ]; then
    # SMP via the BootROM mailbox (+ mailbox clear before restart, rk3228-smp-reset.c), the
    # DesignWare watchdog, SPI controller (NOR /data), thermal sensor and the PWM regulators.
    printf 'CONFIG_DW_WATCHDOG=y\nCONFIG_SPI_ROCKCHIP=y\nCONFIG_ROCKCHIP_THERMAL=y\nCONFIG_PWM_ROCKCHIP=y\n' >> "$KCONFIG"
    cp "$board/kernel/rk3228-smp-reset.c" "$linux/drivers/soc/rockchip/rk3228-smp-reset.c"
    grep -q '^obj-$(CONFIG_SMP) += rk3228-smp-reset.o$' "$socmk" || echo 'obj-$(CONFIG_SMP) += rk3228-smp-reset.o' >> "$socmk"
else
    sed -i '/^obj-$(CONFIG_SMP) += rk3228-smp-reset.o$/d' "$socmk"
fi
if [ "$LAB" = 1 ]; then
    # LAB-ONLY. RK3228 power domains are idle-only (no power switch): genpd's
    # NoC idle requests (GRF_SOC_CON3) survive the watchdog and second global
    # resets, after which MaskROM USB times out (bisected 12 Nov: restoring
    # only GRF 0x40c fixes it). Never idle such domains, so hangs recover.
    grep -q LAB_NO_NOC_IDLE "$pmd" || sed -i '/^static int rockchip_pd_power(struct rockchip_pm_domain \*pd, bool power_on)$/,/^\tguard(mutex)(&pmu->mutex);$/ s/^\tguard(mutex)(&pmu->mutex);$/\tguard(mutex)(\&pmu->mutex);\n\n\tif (!power_on \&\& !pd->info->pwr_mask) \/* LAB_NO_NOC_IDLE *\/\n\t\treturn 0;/' "$pmd"
    grep -q LAB_NO_NOC_IDLE "$pmd"
    # LAB-ONLY. Allow-listed register debugfs + MaskROM restart handler.
    cp "$board/kernel/labreg.c" "$linux/drivers/soc/rockchip/labreg.c"
    grep -q '^obj-y += labreg.o$' "$socmk" || echo 'obj-y += labreg.o' >> "$socmk"
else
    sed -i '/^obj-y += labreg.o$/d' "$socmk"
    sed -i '/LAB_NO_NOC_IDLE/,+1d' "$pmd"
fi
