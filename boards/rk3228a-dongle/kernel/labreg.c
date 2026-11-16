// SPDX-License-Identifier: GPL-2.0
/*
 * LAB-ONLY (built only with LAB=1): register access and MaskROM-recovery
 * restart for the RK3228A RAM-boot lab kernel. Not for production images.
 *
 * /sys/kernel/debug/labreg:
 *   echo "r ADDR [N]" > labreg; cat labreg   -> N (1..256) words from ADDR
 *   echo "w ADDR VALUE" > labreg            -> one 32-bit write
 *
 * Only always-on, allow-listed blocks are mapped: clock/reset, GRF, PWM,
 * TSADC, timers, watchdog, the DDR controller/PHY and the NoC QoS registers. Flash, MMC, SPI, NAND,
 * efuse and every other block are refused, so this is not /dev/mem.
 */
#include <linux/debugfs.h>
#include <linux/delay.h>
#include <linux/init.h>
#include <linux/io.h>
#include <linux/kernel.h>
#include <linux/mutex.h>
#include <linux/notifier.h>
#include <linux/reboot.h>
#include <linux/seq_file.h>
#include <linux/string.h>
#include <linux/uaccess.h>

struct labreg_range {
	const char *name;
	u32 base;
	u32 size;
	void __iomem *va;
};

static struct labreg_range labreg_ranges[] = {
	{ "sgrf_soc", 0x10140000, 0x100 },	/* secure GRF: slave/DMA security (SOC_CON, BUSDMAC_CON) */
	{ "sgrf",     0x10150000, 0x1000 },
	{ "grf",      0x11000000, 0x1000 },
	{ "wdt",      0x110a0000, 0x100 },
	{ "pwm",      0x110b0000, 0x100 },
	{ "timer",    0x110c0000, 0x100 },
	{ "cru",      0x110e0000, 0x1000 },
	{ "tsadc",    0x11150000, 0x100 },
	{ "ddr_pctl", 0x11200000, 0x400 },
	{ "ddr_phy",  0x12000000, 0x400 },
	/* NoC QoS generators (rk322x.dtsi qos@...): priority/mode/bandwidth of
	 * the bus masters' DDR traffic. */
	{ "qos_vio",  0x31030000, 0x400 },
	{ "qos_vpu",  0x31040000, 0x100 },
	{ "qos_gpu",  0x31050000, 0x100 },
	{ "qos_vop",  0x31060000, 0x100 },
	{ "qos_rkvdec", 0x31070000, 0x100 },
	/* DDR memory scheduler (NoC service_msch, U-Boot rk322x-u-boot.dtsi):
	 * ddrconf, ddrtiming, ddrmode, readlatency, activate, devtodev. */
	{ "msch",     0x31090000, 0x40 },
	{ "snap",     0x67d00000, 0x2000 },	/* handoff GRF/CRU copy (reserved RAM) */
	{ "crumbs",   0x67d03000, 0x40 },	/* flash handoff breadcrumbs (+0x18: DDR reclock status) */
};

static DEFINE_MUTEX(labreg_lock);
static u32 labreg_addr = 0x110e0150; /* CRU_GLB_RST_ST */
static u32 labreg_count = 1;
/*
 * "reset fst" | "reset snd" | "reset snd-restore" via debugfs: which global
 * soft reset to issue; snd-restore first puts the hiword-masked GRF/CRU
 * registers Linux changed back to the handoff snapshot.
 */
static bool labreg_reset_fst = true;
static bool labreg_reset_restore;
/* restore groups (hex mask after "reset snd-restore"): 1 CRU gates con0-2
 * (UART/I2S/SPDIF/VOP src), 2 CRU con3+con15 (VDEC/VPU), 4 CRU con7 + soft
 * resets (GPU), 8 CRU con8 (bus DMAC), 0x10 GRF. Default: all. */
static u32 labreg_restore_mask = 0x1f;

static int labreg_find(u32 addr)
{
	int i;

	if (addr & 3)
		return -1;
	for (i = 0; i < ARRAY_SIZE(labreg_ranges); i++) {
		struct labreg_range *r = &labreg_ranges[i];

		if (r->va && addr >= r->base && addr - r->base <= r->size - 4)
			return i;
	}
	return -1;
}

static int labreg_show(struct seq_file *s, void *unused)
{
	u32 i, addr;
	int idx;

	mutex_lock(&labreg_lock);
	idx = labreg_find(labreg_addr);
	for (i = 0; idx >= 0 && i < labreg_count; i++) {
		struct labreg_range *r = &labreg_ranges[idx];

		addr = labreg_addr + 4 * i;
		seq_printf(s, "%08x %08x %s+%03x\n", addr,
			   readl(r->va + (addr - r->base)), r->name, addr - r->base);
	}
	mutex_unlock(&labreg_lock);
	return 0;
}

static int labreg_open(struct inode *inode, struct file *file)
{
	return single_open(file, labreg_show, NULL);
}

static ssize_t labreg_write(struct file *file, const char __user *ubuf,
			    size_t len, loff_t *ppos)
{
	char buf[64];
	u32 addr, value, count = 1;
	int idx, n;

	if (len == 0 || len >= sizeof(buf))
		return -EINVAL;
	if (copy_from_user(buf, ubuf, len))
		return -EFAULT;
	buf[len] = 0;

	if (sscanf(buf, "w %x %x", &addr, &value) == 2) {
		idx = labreg_find(addr);
		if (idx < 0)
			return -EPERM;
		mutex_lock(&labreg_lock);
		writel(value, labreg_ranges[idx].va + (addr - labreg_ranges[idx].base));
		mutex_unlock(&labreg_lock);
		pr_info("labreg: wrote %08x to %08x\n", value, addr);
		return len;
	}
	if (!strncmp(buf, "reset fst", 9) || !strncmp(buf, "reset snd", 9)) {
		labreg_reset_fst = buf[6] == 'f';
		labreg_reset_restore = !strncmp(buf, "reset snd-restore", 17);
		labreg_restore_mask = 0x1f;
		if (labreg_reset_restore && sscanf(buf + 17, " %x", &value) == 1)
			labreg_restore_mask = value;
		pr_info("labreg: restart uses the %s global reset (restore mask %x)\n",
			labreg_reset_fst ? "first" : "second", labreg_reset_restore ? labreg_restore_mask : 0);
		return len;
	}
	n = sscanf(buf, "r %x %u", &addr, &count);
	if (n < 1)
		return -EINVAL;
	if (count == 0 || count > 256)
		return -EINVAL;
	idx = labreg_find(addr);
	if (idx < 0 || labreg_find(addr + 4 * (count - 1)) != idx)
		return -EPERM;
	mutex_lock(&labreg_lock);
	labreg_addr = addr;
	labreg_count = count;
	mutex_unlock(&labreg_lock);
	return len;
}

static const struct file_operations labreg_fops = {
	.owner = THIS_MODULE,
	.open = labreg_open,
	.read = seq_read,
	.llseek = seq_lseek,
	.release = single_release,
	.write = labreg_write,
};

/*
 * Lab recovery: reset into MaskROM (flag set by linux_entry.S). The second
 * global reset keeps GRF, internal SRAM and (as the stock loader's own
 * "reboot to maskrom" implies) some CRU/GIC state, so undo what Linux changed
 * that the BootROM depends on, then reset:
 *  (the SMP mailbox is cleared first by board/rk3228-smp-reset.c);
 *  - APLL/CPLL/GPLL to slow mode (stock writes CRU_MODE_CON before resetting);
 *  - GIC distributor off;
 *  - OTG/host USB PHY out of GRF-controlled suspend (u2phy 0x760/0x764/0x768);
 *  - hard reset of the DWC2 OTG controller and USB PHY (CRU SOFTRST_CON4
 *    SRST_USBOTG/SRST_OTGC bits 5/6, SRST_USBPOR0 bit 13), as the stock loader
 *    does before using USB: Linux's FIFO sizes/GUSBCFG survive a core soft
 *    reset and (observed) MaskROM then enumerates but never accepts EP0 OUT data.
 */
static void __iomem *labreg_gicd;

static int labreg_restart(struct notifier_block *nb, unsigned long mode, void *cmd)
{
	void __iomem *grf = labreg_ranges[1].va;
	void __iomem *cru = labreg_ranges[5].va;

	if (labreg_reset_restore && grf && cru && labreg_ranges[9].va) {
		/* Hiword-masked registers only: GRF iomux/pull (0x000-0x1fc),
		 * SOC_CON (0x400-0x41c), USB irq enable 0x680, PHY 0x760-0x768,
		 * 0x884; CRU gates/softresets (0x0d0-0x13c). */
		static const u16 grf_ranges[][2] = {
			{ 0x000, 0x1fc }, { 0x400, 0x41c }, { 0x680, 0x680 },
			{ 0x760, 0x768 }, { 0x884, 0x884 },
		};
		void __iomem *snap = labreg_ranges[9].va;
		u32 off, v;
		int r;

		for (off = 0x0d0; off <= 0x13c; off += 4) {
			u32 con = (off - 0x0d0) / 4, group;

			if (con <= 2)
				group = 1;
			else if (con == 3 || con == 15)
				group = 2;
			else if (con == 8)
				group = 8;
			else
				group = 4;	/* con7 (aclk_gpu), other gates, soft resets */
			if (!(labreg_restore_mask & group))
				continue;
			v = readl(snap + 0x1000 + off) & 0xffff;
			if ((readl(cru + off) & 0xffff) != v)
				writel(0xffff0000 | v, cru + off);
		}
		for (r = 0; (labreg_restore_mask & 0x10) && r < ARRAY_SIZE(grf_ranges); r++)
			for (off = grf_ranges[r][0]; off <= grf_ranges[r][1]; off += 4) {
				v = readl(snap + off) & 0xffff;
				if ((readl(grf + off) & 0xffff) != v)
					writel(0xffff0000 | v, grf + off);
			}
		dsb(sy);
	}
	if (labreg_gicd)
		writel(0, labreg_gicd);
	if (grf && !labreg_reset_restore) {
		writel(0xffff0000, grf + 0x760);
		writel(0xffff0000, grf + 0x764);
		writel(0x00100000, grf + 0x768);
	}
	dsb(sy);
	if (cru) {
		writel(0x20602060, cru + 0x120);	/* assert OTG, OTGC, USBPOR0 */
		dsb(sy);
		udelay(200);
		writel(0x20600000, cru + 0x120);	/* deassert */
		dsb(sy);
		udelay(2000);
		writel(0x11010000, cru + 0x40);	/* APLL, CPLL, GPLL slow */
		dsb(sy);
		if (labreg_reset_fst)
			writel(0xfdb9, cru + 0x1f0);	/* first global soft reset */
		else
			writel(0xeca8, cru + 0x1f4);	/* second global soft reset */
		dsb(sy);
	}
	return NOTIFY_DONE;
}

static struct notifier_block labreg_restart_nb = {
	.notifier_call = labreg_restart,
	.priority = 250,	/* after rk3228-smp-reset (255) cleared the mailbox */
};

static int __init labreg_init(void)
{
	int i;

	for (i = 0; i < ARRAY_SIZE(labreg_ranges); i++)
		labreg_ranges[i].va = ioremap(labreg_ranges[i].base, labreg_ranges[i].size);
	labreg_gicd = ioremap(0x32011000, 0x10);
	debugfs_create_file("labreg", 0600, NULL, NULL, &labreg_fops);
	register_restart_handler(&labreg_restart_nb);
	return 0;
}
late_initcall(labreg_init);
