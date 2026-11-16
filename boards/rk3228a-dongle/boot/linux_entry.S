/* ARMv7 RAM-only Linux handoff. Host verifies SVC and identity mappings first.
 * Kernel: 0x64000000 (zImage), FDT: 0x68000000.
 * No SPI, flash, efuse or power rail access. DDR controller: TREFI/idle settings, and
 * with DDR_RECLOCK (flash entries) the 300 -> 528 MHz reclock in board/ddr_reclock.c.
 * LAB_RECOVERY (build-host-tools.sh LAB=1, the default): MaskROM-on-reset
 * flag, watchdog pre-arm and GRF/CRU snapshot for the hands-off lab loop.
 */
.syntax unified
.cpu cortex-a7
.arm
.section .text.start,"ax",%progbits
.global stage_entry
.type stage_entry,%function
stage_entry:
    mov r12, #0
    mcr p15, 0, r12, c7, c5, 0
    mcr p15, 0, r12, c7, c5, 6
    dsb sy
    isb sy
#ifdef FLASH_ENTRY
    /* Flash boot (boards/rk3228a-dongle/flash): entered from the stock miniloader at the
     * trust load address 0x68400000 (blx, r1 = kernel load address). A refusal
     * returns to the miniloader, which then reboots to MaskROM; keep its
     * callee-saved registers. Breadcrumbs at 0x67d03000 (reserved, no-map). */
    push {r4-r11, lr}
    ldr r4, =0x67d03000
    ldr r5, =0x48534c46        /* "FLSH" */
    str r5, [r4]
    mrs r5, cpsr
    str r5, [r4, #4]
    mrc p15, 0, r5, c1, c0, 0  /* SCTLR as left by the miniloader */
    str r5, [r4, #8]
    str lr, [r4, #12]
    str r1, [r4, #16]
    mov r5, #1                 /* progress: entered */
    str r5, [r4, #20]
    dsb sy
    mcr p15, 0, r4, c7, c14, 1 /* clean+invalidate the breadcrumb line */
    dsb sy
#endif
    /* Refuse unsupported entry conditions, preserving the helper. */
    mrs r0, cpsr
    and r0, r0, #0x1f
    cmp r0, #0x13
    bne abort
#ifdef FLASH_ENTRY
    /* Embedded DTB (appended right after this code) must be a sane FDT, and
     * the miniloader must have placed a zImage at 0x64000000. Copy the DTB
     * to 0x68000000, where Linux gets it, as in the RAM boot. */
    ldr r4, =fdt_blob
    ldr r5, [r4]
    ldr r6, =0xedfe0dd0
    cmp r5, r6
    bne abort
    ldr r5, [r4, #4]
    rev r5, r5
    cmp r5, #0x40
    blo abort
    ldr r6, =0x10000
    cmp r5, r6
    bhi abort
    ldr r6, =0x64000000
    ldr r7, [r6, #0x24]
    ldr r8, =0x016f2818
    cmp r7, r8
    bne abort
    ldr r6, =0x68000000
4:  ldr r7, [r4], #4
    str r7, [r6], #4
    subs r5, r5, #4
    bgt 4b
    ldr r4, =0x67d03000
    mov r5, #2                 /* progress: checks passed, DTB copied */
    str r5, [r4, #20]
    dsb sy
    mcr p15, 0, r4, c7, c14, 1
    dsb sy
#endif
    /* Match upstream U-Boot RK322x arch_cpu_init (XPL): disable the DDR
     * secure-region restriction so non-secure display/IOMMU DMA can read RAM.
     * SGRF_SOC_CON0[14], Rockchip high-half write mask. Volatile, no efuses.
     */
    ldr r0, =0x10150000
    ldr r1, =0x40000000
    str r1, [r0]
    dsb sy
    ldr r1, [r0]
    tst r1, #0x4000
    bne abort
    /* The PL330 DMA controller is a non-secure master on this SoC (CR0 boot
     * manager NS), while the secure GRF marks the peripherals as secure-only
     * slaves at reset (SGRF_SOC_CON6..10 read 1, ffff, ffff, ffff, 1f). Its
     * writes to the I2S FIFO then fault ("dma-pl330: Reset Channel", CSR
     * 0x20400f, FTC data write error) and HDMI audio never plays. Open every
     * slave to non-secure masters, as OP-TEE does for RK322x
     * (platform_secure_init). High-half write mask. Verified live 14 Nov 2026:
     * the faults stop and sound plays. Volatile, no efuses.
     */
    ldr r0, =0x10140000
    ldr r1, =0xffff0000
    str r1, [r0, #0x18]
    str r1, [r0, #0x1c]
    str r1, [r0, #0x20]
    str r1, [r0, #0x24]
    str r1, [r0, #0x28]
    dsb sy
    /* Rockchip next-dev U-Boot rk322x arch_cpu_init also selects the HDMI
     * PHY output as the display clock source: CRU_MISC_CON[13] = 0.
     * Cached reference commit 1c535d65b8509f388d09e49fb6961f49fda35a1d.
     */
    ldr r0, =0x110e0134
    ldr r1, =0x20000000
    str r1, [r0]
    dsb sy
    ldr r1, [r0]
    tst r1, #0x2000
    bne abort
#ifdef LAB_RECOVERY
    /* Lab recovery: any later soft reset re-enters MaskROM instead of the
     * stock firmware in SPI NOR. The stock miniloader's "reboot to maskrom"
     * writes BOOT_BROM_DOWNLOAD (0xEF08A53C) to GRF_OS_REG0 (0x110005c8) and
     * then issues the second global soft reset (CRU 0x1f4 = 0xeca8), which keeps
     * GRF. Power-on reset clears GRF, so a power cycle still boots the stock
     * firmware. Volatile register only. */
    ldr r0, =0x110005c8
    ldr r1, =0xef08a53c
    str r1, [r0]
    dsb sy
    ldr r2, [r0]
    cmp r2, r1
    bne abort
    /* Arm the DesignWare watchdog (0x110a0000) at its longest period
     * (TOP=TOP_INIT=15: 2^31 pclk cycles, ~22 s at the helper's 96 MHz pclk_cpu)
     * in reset mode, so a hang before Linux's dw_wdt driver takes over also
     * resets into MaskROM. Linux finds it running and keeps it pinged. */
    ldr r0, =0x110a0000
    mov r1, #0xff
    str r1, [r0, #0x04]  /* WDT_TORR */
    mov r1, #0x01
    str r1, [r0, #0x00]  /* WDT_CR: enable, RMOD=0 (system reset) */
    mov r1, #0x76
    str r1, [r0, #0x0c]  /* WDT_CRR: restart the counter */
    dsb sy
#endif /* LAB_RECOVERY */
    cpsid if
#ifdef FLASH_NO_DWC2
    /* Flash boot: the miniloader never started USB; its clock may be gated,
     * so do not touch DWC2 (Linux's dwc2 driver resets the core itself). */
    b clean_cache
#endif
    /* Disconnect and reset DWC2, ending helper USB/DMA before Linux. */
    ldr r0, =0x30040000
    ldr r1, [r0, #0x804]
    orr r1, r1, #2
    str r1, [r0, #0x804]
    mov r1, #0
    str r1, [r0, #0x18]  /* GINTMSK */
    str r1, [r0, #0x08]  /* GAHBCFG: global IRQ and DMA disabled */
    dsb sy
    /* Wait for outstanding AHB transfers before resetting the core. */
    ldr r2, =1000000
1:  ldr r1, [r0, #0x10]
    tst r1, #0x80000000
    bne 2f
    subs r2, r2, #1
    bne 1b
    b stop
2:  mov r1, #1
    str r1, [r0, #0x10]
    ldr r2, =1000000
3:  ldr r1, [r0, #0x10]
    tst r1, #1
    beq clean_cache
    subs r2, r2, #1
    bne 3b
    b stop
clean_cache:
    /* Clean and invalidate every data/unified cache by set and way. */
    dmb sy
    mrc p15, 1, r0, c0, c0, 1  /* CLIDR */
    ands r3, r0, #0x07000000
    mov r3, r3, lsr #23
    beq caches_done
    mov r10, #0
cache_level:
    add r2, r10, r10, lsr #1
    mov r1, r0, lsr r2
    and r1, r1, #7
    cmp r1, #2
    blt next_level
    mcr p15, 2, r10, c0, c0, 0 /* CSSELR */
    isb sy
    mrc p15, 1, r1, c0, c0, 0 /* CCSIDR */
    and r2, r1, #7
    add r2, r2, #4
    ldr r4, =0x3ff
    and r4, r4, r1, lsr #3
    clz r5, r4
    ldr r7, =0x7fff
    and r7, r7, r1, lsr #13
cache_set:
    mov r9, r4
cache_way:
    orr r11, r10, r9, lsl r5
    orr r11, r11, r7, lsl r2
    mcr p15, 0, r11, c7, c14, 2
    subs r9, r9, #1
    bge cache_way
    subs r7, r7, #1
    bge cache_set
next_level:
    add r10, r10, #2
    cmp r3, r10
    bgt cache_level
caches_done:
    mov r0, #0
    mcr p15, 2, r0, c0, c0, 0
    dsb sy
    mrc p15, 0, r1, c1, c0, 0
    bic r1, r1, #1
    bic r1, r1, #4
    bic r1, r1, #0x1000
    mcr p15, 0, r1, c1, c0, 0
    isb sy
    mcr p15, 0, r0, c7, c5, 0
    mcr p15, 0, r0, c7, c5, 6
    mcr p15, 0, r0, c8, c7, 0
    dsb sy
    isb sy
    /* Cortex-A7: ACTLR.SMP (bit 6) resets to 0, and while it is 0 the core
     * treats coherent Normal memory as non-cacheable (measured: 30 MB/s L1
     * reads under Linux). Firmware normally sets it; a CONFIG_SMP=n kernel
     * never does. ARM requires it set with caches/MMU off, i.e. here. We are
     * in secure SVC, so ACTLR is writable. */
#ifdef LAB_RECOVERY
    /* Lab: snapshot GRF and CRU as the USB helper left them (a state the
     * BootROM/helper USB works in) to reserved RAM 0x67d00000 (GRF) and
     * 0x67d01000 (CRU 0x000-0x13c), so Linux can diff what it changed (board/labreg.c "snap"). */
    ldr r0, =0x11000000
    ldr r2, =0x67d00000
    mov r3, #0x1000
    bl snapcopy
    /* CRU: only the implemented PLL/clksel/gate/softrst block; reads of some
     * other CRU offsets stall the bus (seen with the USB helper). */
    ldr r0, =0x110e0000
    ldr r2, =0x67d01000
    mov r3, #0x140
    bl snapcopy
    dsb sy
#endif /* LAB_RECOVERY */
#if defined(DDR_PD_IDLE) || defined(DDR_SR_IDLE) || defined(DDR_TREFI)
    /* DDR controller idle power saving (stock DTB dram_timing: pd_idle=32,
     * sr_idle=12; the rkbin DDR blob leaves both 0 and ignores its own params).
     * MCFG/MCFG1 are writable only in PCTL Config state, where DRAM cannot be
     * accessed, so run the switch from internal SRAM (MMU/caches are off and
     * no other bus master is active at this point). */
#ifndef DDR_PD_IDLE
#define DDR_PD_IDLE 0
#endif
#ifndef DDR_SR_IDLE
#define DDR_SR_IDLE 0
#endif
    ldr r0, =0x11200000
    ldr r2, [r0, #0x80]          /* MCFG: pd_idle in [15:8] */
    bic r2, r2, #0xff00
    orr r2, r2, #(DDR_PD_IDLE << 8)
    ldr r3, [r0, #0x7c]          /* MCFG1: sr_idle in [7:0] */
    bic r3, r3, #0xff
    orr r3, r3, #DDR_SR_IDLE
    /* TREFI (refresh interval, units of 100 ns; blob default 78 = 7.8 us).
     * DDR3 needs 3.9 us (39) above 85 C case temperature. 0 = keep. */
#ifndef DDR_TREFI
#define DDR_TREFI 0
#endif
    ldr r8, [r0, #0xd0]
    ldr r9, =DDR_TREFI
    cmp r9, #0
    movne r8, r9
    adr r4, ddr_cfg_sram
    adr r5, ddr_cfg_sram_end
    ldr r6, =0x10080800
6:  ldr r1, [r4], #4
    str r1, [r6], #4
    cmp r4, r5
    blo 6b
    dsb sy
    isb sy
    ldr r7, =0x67d02000          /* progress record (lab snapshot area) */
    ldr r1, =0xdd100001
    str r1, [r7]
    str r2, [r7, #4]
    str r3, [r7, #8]
    dsb sy
    ldr r6, =0x10080800
    blx r6
    ldr r7, =0x67d02000
    ldr r0, =0x11200000
    ldr r1, =0xdd100002
    str r1, [r7, #0x10]
    ldr r1, [r0, #0x08]
    str r1, [r7, #0x14]
    ldr r1, [r0, #0x80]
    str r1, [r7, #0x18]
    ldr r1, [r0, #0x7c]
    str r1, [r7, #0x1c]
    dsb sy
#endif
#ifdef DDR_RECLOCK
    /* DDR3 300 -> 528 MHz (board/ddr_reclock.c): the stock IDBlock DDR init leaves
     * 300 MHz. The routine and its stack live in internal SRAM (U-Boot's ddr_sram,
     * 0x10081000-0x10088fff): DRAM is in self-refresh while the clock changes. It
     * returns a status word, kept as breadcrumb 0x67d03018. */
    ldr r4, =__ddr_reclock_start
    ldr r5, =__ddr_reclock_end
    ldr r6, =0x10081000
7:  ldr r1, [r4], #4
    str r1, [r6], #4
    cmp r4, r5
    blo 7b
    dsb sy
    isb sy
    ldr r4, =ddr_reclock
    ldr r5, =__ddr_reclock_start
    sub r4, r4, r5
    ldr r6, =0x10081000
    add r6, r6, r4
    mov r10, sp                  /* callee-saved: survives the call */
    ldr sp, =0x10088ff0
    blx r6
    mov sp, r10
    ldr r4, =0x67d03000
    str r0, [r4, #24]
    dsb sy
#endif
    mrc p15, 0, r1, c1, c0, 1
    orr r1, r1, #0x40
    mcr p15, 0, r1, c1, c0, 1
    isb sy
#ifdef FLASH_ENTRY
    ldr r4, =0x67d03000
    mov r5, #3                 /* progress: jumping to Linux (caches off) */
    str r5, [r4, #20]
    dsb sy
#endif
    mov r0, #0
    mvn r1, #0
    ldr r2, =0x68000000
    ldr r3, =0x64000000
    bx r3
#ifdef LAB_RECOVERY
snapcopy:
1:  ldr r1, [r0], #4
    str r1, [r2], #4
    subs r3, r3, #4
    bne 1b
    bx lr
#endif /* LAB_RECOVERY */
#if defined(DDR_PD_IDLE) || defined(DDR_SR_IDLE) || defined(DDR_TREFI)
/* Position-independent; runs from SRAM. r0 = PCTL, r2 = MCFG, r3 = MCFG1,
 * r8 = TREFI. */
ddr_cfg_sram:
    mov r1, #1                   /* SCTL = CFG */
    str r1, [r0, #0x04]
1:  ldr r1, [r0, #0x08]
    and r1, r1, #7
    cmp r1, #1                   /* STAT = CONFIG */
    bne 1b
    str r2, [r0, #0x80]
    str r3, [r0, #0x7c]
    orr r1, r8, #0x80000000      /* TREFI with update bit */
    str r1, [r0, #0xd0]
    mov r1, #2                   /* SCTL = GO */
    str r1, [r0, #0x04]
2:  ldr r1, [r0, #0x08]
    and r1, r1, #7
    cmp r1, #3                   /* STAT = ACCESS */
    bne 2b
    dsb sy
    bx lr
ddr_cfg_sram_end:
#endif
abort:
    mov r0, #1
#ifdef FLASH_ENTRY
    pop {r4-r11, pc}
#else
    bx lr
#endif
stop:
    wfi
    b stop
.size stage_entry, .-stage_entry
#ifdef FLASH_ENTRY
    .ltorg
    /* The packer (boards/rk3228a-dongle/flash/pack-flash.py) appends the DTB here: its own
     * section, which flash_entry.ld places last (after board/ddr_reclock.c). */
    .section .fdt_tail,"a",%progbits
    .balign 16
    .global fdt_blob
fdt_blob:
#endif
