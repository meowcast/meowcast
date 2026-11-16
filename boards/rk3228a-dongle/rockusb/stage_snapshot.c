/* Read-only CPU register snapshot. No writes except the DRAM result buffer. */
typedef unsigned int u32;
static u32 read32(u32 address) { return *(volatile u32 *)address; }
int stage_main(u32 opcode, u32 subcode, void *buffer, u32 length) {
    (void)opcode; (void)subcode; (void)buffer; (void)length;
    volatile u32 *out=(volatile u32 *)0x62000000;
    out[0]=0x50414e53; /* SNAP */
    for (u32 i=0;i<16;i++) out[4+i]=read32(0x11000000+i*4); /* GPIO iomux */
    out[20]=read32(0x11000050); /* GRF con_iomux */
    out[21]=read32(0x110e0040); /* CRU mode */
    out[22]=read32(0x110e00a8); /* CRU clksel[25], SPI clock */
    for (u32 i=0;i<16;i++)out[24+i]=read32(0x110e00d0+i*4); /* clock gates */
    out[1]=0x59414b4f; /* OKAY after all reads complete */
    __asm__ volatile("dsb sy" ::: "memory");
    return 0;
}
