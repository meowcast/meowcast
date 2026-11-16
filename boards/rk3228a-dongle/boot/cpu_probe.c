/* Diagnostic callback: CPU registers, two read-only boot-state MMIO registers,
 * and writes to scratch DRAM. ATS instructions update only CPU translation state.
 */
typedef unsigned int u32;
int stage_main(u32 a, u32 b, void *c, u32 d) {
    (void)a; (void)b; (void)c; (void)d;
    volatile u32 *out = (volatile u32 *)0x62000000;
    out[0] = 0x55504352; /* RCPU */
    u32 v;
    __asm__ volatile("mrs %0, cpsr" : "=r"(v)); out[2] = v;
    __asm__ volatile("mrc p15, 0, %0, c1, c0, 0" : "=r"(v)); out[3] = v;
    __asm__ volatile("mrc p15, 0, %0, c0, c0, 0" : "=r"(v)); out[4] = v;
    __asm__ volatile("mrc p15, 0, %0, c0, c0, 5" : "=r"(v)); out[5] = v;
    __asm__ volatile("mrc p15, 1, %0, c0, c0, 1" : "=r"(v)); out[6] = v;
    __asm__ volatile("mrc p15, 0, %0, c0, c1, 1" : "=r"(v)); out[7] = v;
    __asm__ volatile("mrc p15, 0, %0, c14, c0, 0" : "=r"(v)); out[8] = v;
    __asm__ volatile("mrc p15, 0, %0, c2, c0, 0" : "=r"(v)); out[9] = v;
    __asm__ volatile("mrc p15, 0, %0, c2, c0, 1" : "=r"(v)); out[10] = v;
    __asm__ volatile("mrc p15, 0, %0, c2, c0, 2" : "=r"(v)); out[11] = v;
    const u32 addresses[] = {0x62002000, 0x64000000, 0x68000000, 0x30040000, 0x10150000};
    for (u32 i = 0; i < 5; ++i) {
        v = addresses[i];
        __asm__ volatile("mcr p15, 0, %0, c7, c8, 0\n\tisb sy" :: "r"(v) : "memory");
        __asm__ volatile("mrc p15, 0, %0, c7, c4, 0" : "=r"(v));
        out[12+i] = v;
    }
    // Read only after confirming the mapping exists. U-Boot initializes this
    // volatile DDR security bit before booting a normal Linux system.
    out[17] = (out[16] & 1) ? 0xffffffff : *(volatile u32 *)0x10150000;
    v = 0x110e0134;
    __asm__ volatile("mcr p15, 0, %0, c7, c8, 0\n\tisb sy" :: "r"(v) : "memory");
    __asm__ volatile("mrc p15, 0, %0, c7, c4, 0" : "=r"(v));
    out[19] = v;
    out[18] = (v & 1) ? 0xffffffff : *(volatile u32 *)0x110e0134;
    out[1] = 0x59414b4f; /* OKAY */
    __asm__ volatile("dsb sy" ::: "memory");
    return 0;
}
