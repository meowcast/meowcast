// SPDX-License-Identifier: GPL-2.0
/*
 * RK3228 SMP without a secure monitor (production-relevant, not a lab tool).
 *
 * Secondary cores are started through the BootROM mailbox in internal SRAM
 * ("rockchip,rk3036-smp": SRAM+4 = 0xDEADBEAF, SRAM+8 = entry). SRAM keeps its
 * contents across soft and watchdog resets, so after any warm reset CPUs 1-3
 * would leave the BootROM straight into stale kernel code in DRAM while the
 * next boot stage runs. Clear the mailbox before every restart.
 */
#include <linux/init.h>
#include <linux/io.h>
#include <linux/notifier.h>
#include <linux/of.h>
#include <linux/of_address.h>
#include <linux/reboot.h>

static void __iomem *rk3228_smp_mailbox;

static int rk3228_smp_reset_notify(struct notifier_block *nb, unsigned long mode, void *cmd)
{
	writel(0, rk3228_smp_mailbox + 4);
	writel(0, rk3228_smp_mailbox + 8);
	dsb(sy);
	return NOTIFY_DONE;
}

static struct notifier_block rk3228_smp_reset_nb = {
	.notifier_call = rk3228_smp_reset_notify,
	.priority = 255,	/* before any handler that actually resets */
};

static int __init rk3228_smp_reset_init(void)
{
	struct device_node *np;

	np = of_find_compatible_node(NULL, NULL, "rockchip,rk3066-smp-sram");
	if (!np)
		return 0;
	rk3228_smp_mailbox = of_iomap(np, 0);
	of_node_put(np);
	if (!rk3228_smp_mailbox)
		return -ENOMEM;
	return register_restart_handler(&rk3228_smp_reset_nb);
}
device_initcall(rk3228_smp_reset_init);
