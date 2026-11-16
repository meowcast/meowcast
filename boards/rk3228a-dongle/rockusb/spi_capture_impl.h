/* Source-reviewed CPU SPI reader. Flash bus emits only 0x9f (ID) and 0x03 (read).
 * All other writes configure volatile SPI/pin-mux registers or DRAM.
 * SPI0 register definitions: Rockchip rk_spi.{h,c}; routing: RK322x DTS/pinctrl.
 */
typedef unsigned int u32;
typedef unsigned char u8;
#define SPI 0x11090000u
#define GRF 0x11000000u
#define CRU 0x110e0000u
#define CAPTURE_BASE 0x64000000u
_Static_assert(CAPTURE_BYTES>0 && CAPTURE_BYTES<=16777216u,"Invalid read size");
static u32 rd(u32 a) { return *(volatile u32 *)a; }
static void wr(u32 a,u32 v) { *(volatile u32 *)a=v; }

/* Full duplex PIO with bounded waits and no more transmitted than received+32. */
static int transfer(const u8 *header,u32 header_len,volatile u8 *dest,u32 count) {
    const u32 total=header_len+count;
    if(total==0 || total>65536)return 1;
    wr(SPI+8,0);wr(SPI+4,total-1);wr(SPI+12,1);wr(SPI+8,1);
    u32 tx=0,rx=0,budget=1000000;
    while(rx<total) {
        u32 status=rd(SPI+0x24),progress=0;
        if(tx<total && !(status&2)) {
            wr(SPI+0x400,tx<header_len?header[tx]:0);
            tx++;progress=1;
        }
        if(!(status&8)) {
            u32 value=rd(SPI+0x800);
            if(rx>=header_len)dest[rx-header_len]=(u8)value;
            rx++;progress=1;
        }
        if(progress)budget=1000000;
        else if(!--budget) {wr(SPI+12,0);wr(SPI+8,0);return 2;}
    }
    budget=1000000;
    while((rd(SPI+0x24)&1) && --budget){}
    wr(SPI+12,0);wr(SPI+8,0);
    return budget?0:3;
}

int stage_main(u32 opcode,u32 subcode,void *buffer,u32 length) {
    (void)opcode;(void)subcode;(void)buffer;(void)length;
    volatile u32 *out=(volatile u32 *)0x62000000;
    out[0]=0x52524f4e; /* NORR */ out[1]=0;out[2]=0;out[3]=0;
    out[24]=CAPTURE_BASE;out[25]=CAPTURE_BYTES;
    u32 pins=rd(GRF+0x20),route=rd(GRF+0x50),clock_pin=rd(GRF+8);
    out[4]=pins;out[5]=route;out[6]=clock_pin;
    if(!(route&(1u<<5)) || ((clock_pin>>14)&3)!=2 || (rd(CRU+0xd8)&(1u<<9)) || (rd(CRU+0xf4)&(1u<<6))) {
        out[1]=1;return 0;
    }
    u32 ctrl0=rd(SPI),ctrl1=rd(SPI+4),enable=rd(SPI+8),select=rd(SPI+12);
    u32 baud=rd(SPI+16),imr=rd(SPI+0x2c),dma=rd(SPI+0x3c);
    out[7]=ctrl0;out[8]=ctrl1;out[9]=enable;out[10]=select;out[11]=baud;
    if(rd(SPI+0x24)&1) {out[1]=2;return 0;}
    wr(SPI+8,0);wr(SPI+12,0);
    wr(GRF+0x20,(0x3fu<<16)|0x2a);
    wr(SPI,0x2401);wr(SPI+16,4);wr(SPI+0x2c,0);wr(SPI+0x3c,0);
    u8 command=0x9f;
    volatile u8 *id=(volatile u8 *)(out+20);
    int error=transfer(&command,1,id,3);
    out[3]=error;
    if(error || id[0]!=0xef || id[1]!=0x40 || id[2]!=0x18) out[1]=3;
    else {
        out[1]=4; /* ID verified; capture in progress. */
        for(u32 offset=0;offset<CAPTURE_BYTES;) {
            u32 todo=CAPTURE_BYTES-offset;
            if(todo>16384)todo=16384;
            u8 header[4]={0x03,(u8)(offset>>16),(u8)(offset>>8),(u8)offset};
            error=transfer(header,4,(volatile u8 *)(CAPTURE_BASE+offset),todo);
            if(error) {out[1]=5;out[3]=error;break;}
            offset+=todo;out[2]=offset;
        }
        if(out[2]==CAPTURE_BYTES)out[1]=0x59414b4f; /* OKAY */
    }
    wr(SPI+12,0);wr(SPI+8,0);
    wr(SPI,ctrl0);wr(SPI+4,ctrl1);wr(SPI+16,baud);
    wr(SPI+0x2c,imr);wr(SPI+0x3c,dma);
    wr(GRF+0x20,(0x3fu<<16)|(pins&0x3f));
    wr(SPI+12,select);wr(SPI+8,enable);
    out[26]=rd(GRF+0x20);out[27]=rd(SPI);out[28]=rd(SPI+8);
    __asm__ volatile("dsb sy" ::: "memory");
    return 0;
}
