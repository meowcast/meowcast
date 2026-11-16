/* RK322x SPI0 routed through the spi-1 pins: read JEDEC ID only.
 * Source register layout/bit definitions: Rockchip rk_spi.{h,c}, kernel DTS.
 * No flash write-enable, program, erase, status-write or reset commands. */
typedef unsigned int u32;
static u32 rd(u32 a) { return *(volatile u32 *)a; }
static void wr(u32 a,u32 v) { *(volatile u32 *)a=v; }
#define SPI 0x11090000u
#define GRF 0x11000000u
#define CRU 0x110e0000u
int stage_main(u32 opcode,u32 subcode,void *buffer,u32 length) {
    (void)opcode;(void)subcode;(void)buffer;(void)length;
    volatile u32 *out=(volatile u32 *)0x62000000;
    out[0]=0x44495053; /* SPID */
    out[1]=0;
    u32 pins=rd(GRF+0x20),route=rd(GRF+0x50),clock_pin=rd(GRF+0x08);
    u32 sgate=rd(CRU+0xd8),pgate=rd(CRU+0xf4);
    out[4]=pins;out[5]=route;out[6]=clock_pin;out[7]=sgate;out[8]=pgate;
    if (!(route&(1u<<5)) || ((clock_pin>>14)&3)!=2 || (sgate&(1u<<9)) || (pgate&(1u<<6))) {
        out[1]=1;return 0; /* Don't touch an unexpected configuration. */
    }
    u32 ctrl0=rd(SPI),ctrl1=rd(SPI+4),enable=rd(SPI+8),select=rd(SPI+12);
    u32 baud=rd(SPI+16),imr=rd(SPI+0x2c),dma=rd(SPI+0x3c);
    out[9]=ctrl0;out[10]=ctrl1;out[11]=enable;out[12]=select;out[13]=baud;
    out[14]=rd(SPI+0x24);
    if (out[14]&1) {out[1]=2;return 0;}
    wr(SPI+8,0);wr(SPI+12,0);
    wr(GRF+0x20,(0x3fu<<16)|0x2a); /* GPIO2A0=MISO,A1=MOSI,A2=CS0, mux 2. */
    wr(SPI,0x2401); /* 8-bit, mode 0, master, keep CS, 1-cycle delay, byte I/O. */
    wr(SPI+4,3);wr(SPI+16,128);wr(SPI+0x2c,0);wr(SPI+0x3c,0);
    wr(SPI+12,1);wr(SPI+8,1);
    wr(SPI+0x400,0x9f); /* JEDEC read identification. */
    wr(SPI+0x400,0);wr(SPI+0x400,0);wr(SPI+0x400,0);
    u32 budget=1000000;
    while(rd(SPI+0x20)<4 && --budget){}
    out[15]=budget;out[16]=rd(SPI+0x24);out[17]=rd(SPI+0x20);
    if (budget) {
        for(u32 i=0;i<4;i++)out[20+i]=rd(SPI+0x800)&255;
        out[1]=0x59414b4f; /* OKAY means four received bytes. */
    } else out[1]=3;
    wr(SPI+12,0);wr(SPI+8,0);
    wr(SPI,ctrl0);wr(SPI+4,ctrl1);wr(SPI+16,baud);
    wr(SPI+0x2c,imr);wr(SPI+0x3c,dma);
    wr(GRF+0x20,(0x3fu<<16)|(pins&0x3f));
    wr(SPI+12,select);wr(SPI+8,enable);
    out[24]=rd(GRF+0x20);out[25]=rd(SPI);out[26]=rd(SPI+8);
    __asm__ volatile("dsb sy" ::: "memory");
    return 0;
}
