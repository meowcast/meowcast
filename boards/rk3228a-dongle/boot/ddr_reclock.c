// SPDX-License-Identifier: GPL-2.0
/*
 * DDR3 reclock for flash boots: 300 MHz (stock IDBlock DDR init V1.07, DLL-off mode)
 * -> 528 MHz (the register state rkbin's rk322x DDR blob produces with ddr3_freq=533,
 * memtest-clean on 12 Nov). Called once from the flash handoff (linux_entry.S,
 * DDR_RECLOCK) before Linux, with MMU and caches off and no other bus master active.
 *
 * Runs from internal SRAM (0x10081000, U-Boot's ddr_sram) with its stack in SRAM: DRAM
 * is in self-refresh for part of it. So this file must not touch DRAM, which includes
 * constant tables: every value below is an immediate (no .rodata, no .data; the build
 * checks the section is self-contained).
 *
 * Sequence (frequency change through self-refresh keeps the DRAM contents):
 *   1. PCTL Config: 528 MHz timings (diff of the two register dumps), MCFG 2T,
 *      DFI read-enable latency; PHY CL/AL and CWL.
 *   2. PCTL Access -> Low_power: DRAM in self-refresh.
 *   3. DPLL 600 -> 1056 MHz (DDR clock = DPLL / 2); PHY DLL out of bypass; PHY soft
 *      reset so its DLL relocks at the new clock.
 *   4. Wake up -> Config; DDR3 mode registers for DLL-on operation (MR2 CWL6, MR3,
 *      MR1 DLL on + 40 ohm + RTT_NOM 120, MR0 CL8 WR8 + DLL reset), ZQCL.
 *   5. DQS gate training (PHY squelch calibration), as U-Boot's sdram_rk322x.c.
 *   6. Access.
 * If training fails, the same steps run again with the values captured at entry
 * (back to the stock 300 MHz DLL-off configuration) and the result says so.
 *
 * Register sources: U-Boot drivers/ram/rockchip/sdram_rk322x.c (GPL-2.0+) and register
 * dumps of both states taken on this board (labreg, 13 Nov).
 *
 * Returns: 0x52800000 | training result   reclocked to 528 MHz
 *          0x5300xxxx                     stock state not recognised: nothing changed
 *          0xfa1100xx | result            528 training failed, back at 300 MHz
 */
#include <stdint.h>

/* All code of this file goes to one section, which the handoff copies to SRAM. */
#pragma clang section text = ".text.ddr_reclock" rodata = ".text.ddr_reclock.ro" data = ".ddr_reclock.forbidden.data" bss = ".ddr_reclock.forbidden.bss"

#define PCTL 0x11200000u
#define PHY 0x12000000u
#define CRU 0x110e0000u
#define GRF 0x11000000u

/* PCTL */
#define SCTL 0x004
#define STAT 0x008
#define MCMD 0x040
#define MCFG 0x080
#define TREFI 0x0d0
#define DFITRDDATAEN 0x260
#define ST_CONFIG 1
#define ST_ACCESS 3
#define ST_LOW_POWER 5
#define CMD_CFG 1
#define CMD_GO 2
#define CMD_SLEEP 3
#define CMD_WAKEUP 4
#define DESELECT 0
#define PREA 1
#define REF 2
#define MRS 3
#define ZQCL 5
/* PHY register n is at PHY + 4 * n */
#define PHY_REG(n) (PHY + 4u * (n))
/* CRU */
#define DPLL_CON0 0x00c
#define DPLL_CON1 0x010
#define MODE_CON 0x040

#define R(a) (*(volatile uint32_t *)(a))

static inline uint64_t ticks(void)
{
	uint32_t lo, hi;

	__asm__ volatile("isb\n\tmrrc p15, 0, %0, %1, c14" : "=r"(lo), "=r"(hi));
	return ((uint64_t)hi << 32) | lo;
}

/* Generic timer (24 MHz system counter, already running: Linux relies on it). The
 * guard ends the wait if the counter were stopped (each pass is many cycles). */
static void udelay(uint32_t us)
{
	uint64_t t0 = ticks(), n = (uint64_t)us * 24;
	uint32_t guard = us * 400 + 400;

	while (ticks() - t0 < n && --guard)
		;
}

static int wait_stat(uint32_t want)
{
	uint32_t guard = 1000000;

	while ((R(PCTL + STAT) & 7) != want)
		if (!--guard)
			return -1;
	return 0;
}

static int sctl(uint32_t cmd, uint32_t want)
{
	R(PCTL + SCTL) = cmd;
	return wait_stat(want);
}

static void command(uint32_t cmd, uint32_t arg)
{
	uint32_t guard = 100000;

	R(PCTL + MCMD) = 0x80000000u | (3u << 20) | arg | cmd; /* START, both ranks */
	udelay(1);
	while ((R(PCTL + MCMD) & 0x80000000u) && --guard)
		;
}

static void mrs(uint32_t mr, uint32_t value)
{
	command(MRS, (mr & 7) << 17 | (value & 0x1fff) << 4);
}

/* Everything that differs between the two DDR states (13 Nov register dumps). */
struct state {
	uint32_t dpll_con0;
	uint32_t mcfg, dfitrddataen;
	uint32_t togcnt1u, togcnt100n, trfc, trp, trtw, tcl, tcwl, tras, trc, trcd;
	uint32_t twr, txp, txpdll, tcksre, tcksrx, tcke, trstl, tckesr, tref_mem;
	uint32_t phy_cl_al, phy_cwl, phy_bypass;
	uint32_t mr0, mr1, mr2, dll_on;
};

static void capture(struct state *s)
{
	s->dpll_con0 = R(CRU + DPLL_CON0) & 0x7fff;
	s->mcfg = R(PCTL + MCFG);
	s->dfitrddataen = R(PCTL + DFITRDDATAEN);
	s->togcnt1u = R(PCTL + 0x0c0);
	s->togcnt100n = R(PCTL + 0x0cc);
	s->trfc = R(PCTL + 0x0d8);
	s->trp = R(PCTL + 0x0dc);
	s->trtw = R(PCTL + 0x0e0);
	s->tcl = R(PCTL + 0x0e8);
	s->tcwl = R(PCTL + 0x0ec);
	s->tras = R(PCTL + 0x0f0);
	s->trc = R(PCTL + 0x0f4);
	s->trcd = R(PCTL + 0x0f8);
	s->twr = R(PCTL + 0x104);
	s->txp = R(PCTL + 0x110);
	s->txpdll = R(PCTL + 0x114);
	s->tcksre = R(PCTL + 0x124);
	s->tcksrx = R(PCTL + 0x128);
	s->tcke = R(PCTL + 0x12c);
	s->trstl = R(PCTL + 0x134);
	s->tckesr = R(PCTL + 0x140);
	s->tref_mem = R(PCTL + 0x148);
	s->phy_cl_al = R(PHY_REG(0x0b));
	s->phy_cwl = R(PHY_REG(0x0c));
	s->phy_bypass = R(PHY_REG(0xa4));
	/* Stock V1.07 = U-Boot rk3229-evb 300 MHz params: MR0 CL6 WR5, MR1 DLL off. */
	s->mr0 = 0x220;
	s->mr1 = 0x1;
	s->mr2 = 0x0;
	s->dll_on = 0;
}

static void target_528(struct state *s)
{
	s->dpll_con0 = 0x102c; /* postdiv1 1, fbdiv 44: 24 MHz * 44 = 1056 MHz */
	s->mcfg = 0x00050029;  /* + two_t_en (rkbin ddr_2t=1) */
	s->dfitrddataen = 2;
	s->togcnt1u = 0x10a;
	s->togcnt100n = 0x1a;
	s->trfc = 0xbb;
	s->trp = 8;
	s->trtw = 4;
	s->tcl = 8;
	s->tcwl = 6;
	s->tras = 0x14;
	s->trc = 0x1c;
	s->trcd = 8;
	s->twr = 8;
	s->txp = 4;
	s->txpdll = 0xd;
	s->tcksre = 6;
	s->tcksrx = 6;
	s->tcke = 4;
	s->trstl = 0x36;
	s->tckesr = 5;
	s->tref_mem = 0x7ec;
	s->phy_cl_al = 0x80;
	s->phy_cwl = 6;
	s->phy_bypass = 0; /* DLL in use above 400 MHz (U-Boot phy_dll_bypass_set) */
	s->mr0 = 0x840;    /* BL8, CL8 (A6:4=100), WR8 (A11:9=100); DLL reset added below */
	s->mr1 = 0x40;     /* DLL on, 40 ohm (RZQ/6), RTT_NOM 120 ohm (RZQ/2) */
	s->mr2 = 0x08;     /* CWL 6 */
	s->dll_on = 1;
}

static uint32_t data_training(void)
{
	uint32_t trefi = R(PCTL + TREFI), ret, bw = (R(PHY_REG(0)) >> 4) & 0xf;

	R(PCTL + TREFI) = 0x80000000u; /* auto refresh off during training */
	R(PHY_REG(2)) = (R(PHY_REG(2)) & ~0x30u) | 0x20; /* squelch calibration, CS0 */
	R(PHY_REG(2)) |= 1;                               /* start */
	udelay(30);
	ret = R(PHY_REG(0xff));
	R(PHY_REG(2)) &= ~1u;
	command(PREA, 0); /* make up for the refreshes skipped meanwhile */
	command(REF, 0);
	R(PCTL + TREFI) = trefi | 0x80000000u;
	if (ret & 0x10)
		return 0x100 | ret;
	return ((ret & 0xf) == bw) ? ret : (0x200 | ret);
}

/* Switch the running DDR to state *s. Returns the training result (bit 8/9 = failed)
 * or 0x400 | step if the PCTL state machine did not respond. */
static uint32_t apply(const struct state *s)
{
	/* 1. Timings, in Config state (no DRAM access meanwhile). */
	if (sctl(CMD_CFG, ST_CONFIG))
		return 0x401;
	R(PCTL + MCFG) = s->mcfg;
	R(PCTL + DFITRDDATAEN) = s->dfitrddataen;
	R(PCTL + 0x0c0) = s->togcnt1u;
	R(PCTL + 0x0cc) = s->togcnt100n;
	R(PCTL + 0x0d8) = s->trfc;
	R(PCTL + 0x0dc) = s->trp;
	R(PCTL + 0x0e0) = s->trtw;
	R(PCTL + 0x0e8) = s->tcl;
	R(PCTL + 0x0ec) = s->tcwl;
	R(PCTL + 0x0f0) = s->tras;
	R(PCTL + 0x0f4) = s->trc;
	R(PCTL + 0x0f8) = s->trcd;
	R(PCTL + 0x104) = s->twr;
	R(PCTL + 0x110) = s->txp;
	R(PCTL + 0x114) = s->txpdll;
	R(PCTL + 0x124) = s->tcksre;
	R(PCTL + 0x128) = s->tcksrx;
	R(PCTL + 0x12c) = s->tcke;
	R(PCTL + 0x134) = s->trstl;
	R(PCTL + 0x140) = s->tckesr;
	R(PCTL + 0x148) = s->tref_mem;
	R(PHY_REG(0x0b)) = s->phy_cl_al;
	R(PHY_REG(0x0c)) = s->phy_cwl;

	/* 2. Self-refresh. */
	if (sctl(CMD_GO, ST_ACCESS))
		return 0x402;
	if (sctl(CMD_SLEEP, ST_LOW_POWER))
		return 0x403;

	/* 3. DPLL: slow mode, power down, new dividers, power up, lock, normal mode
	 *    (hiword-masked registers). */
	R(CRU + MODE_CON) = (1u << 20) | (0u << 4);
	R(CRU + DPLL_CON1) = (1u << 29) | (1u << 13);
	R(CRU + DPLL_CON0) = 0x7fff0000u | s->dpll_con0;
	R(CRU + DPLL_CON1) = (1u << 29) | (0u << 13);
	{
		uint32_t guard = 2000;

		while (!(R(CRU + DPLL_CON1) & (1u << 10)) && --guard)
			udelay(1);
	}
	udelay(5);
	R(CRU + MODE_CON) = (1u << 20) | (1u << 4);
	udelay(1);
	/* PHY DLL bypass for the new clock, then PHY soft reset (U-Boot phy_softreset; the
	 * GRF buffer enable keeps the DRAM pins driven by the core meanwhile). */
	R(PHY_REG(0xa4)) = (R(PHY_REG(0xa4)) & ~0x1fu) | (s->phy_bypass & 0x1f);
	R(GRF + 0x400) = (1u << 24) | (0u << 8); /* DDRPHY_BUFFEREN_CORE_EN */
	R(PHY_REG(0)) &= ~(3u << 2);
	udelay(1);
	R(PHY_REG(0)) |= 1u << 2; /* analog out of reset */
	udelay(5);
	R(PHY_REG(0)) |= 1u << 3; /* digital out of reset */
	R(GRF + 0x400) = (1u << 24) | (1u << 8); /* DDRPHY_BUFFEREN_CORE_DIS */
	udelay(10);                              /* PHY DLL lock */

	/* 4. Out of self-refresh, mode registers for the new clock. */
	if (sctl(CMD_WAKEUP, ST_ACCESS))
		return 0x404;
	if (sctl(CMD_CFG, ST_CONFIG))
		return 0x405;
	command(DESELECT, 0);
	udelay(1);
	command(PREA, 0);
	mrs(2, s->mr2);
	mrs(3, 0);
	mrs(1, s->mr1);
	mrs(0, s->mr0 | (s->dll_on ? 0x100 : 0)); /* DLL reset when the DLL is on */
	command(ZQCL, 0);
	udelay(10); /* tDLLK 512 nCK, tZQinit 512 nCK: about 1 us each at 528 MHz */

	/* 5. DQS gate training. */
	{
		uint32_t t = data_training();

		/* 6. Back to normal operation. */
		if (sctl(CMD_GO, ST_ACCESS))
			return 0x406;
		return t;
	}
}

__attribute__((used))
uint32_t ddr_reclock(void)
{
	struct state stock, fast;

	/* Only from the exact stock 300 MHz state (DPLL 600 MHz, CL6, PHY DLL bypass). */
	if ((R(PCTL + STAT) & 7) != ST_ACCESS)
		return 0x53000001;
	if ((R(CRU + DPLL_CON0) & 0x7fff) != 0x304b || (R(CRU + DPLL_CON1) & 0x7fff) != 0x1441)
		return 0x53000002;
	if (R(PCTL + 0x0e8) != 6 || R(PCTL + 0x0c0) != 0x96 || (R(PHY_REG(0xa4)) & 0x1f) != 0x1f)
		return 0x53000003;
	if (((R(PHY_REG(0)) >> 4) & 0xf) != 0xf) /* 32-bit bus */
		return 0x53000004;
	capture(&stock);
	target_528(&fast); /* sets every field (no struct copy: it could call memcpy) */
	{
		uint32_t t = apply(&fast);

		if (!(t & 0x700))
			return 0x52800000u | t;
		/* Training or state change failed at 528: back to the stock state. */
		apply(&stock);
		return 0xfa110000u | t;
	}
}
