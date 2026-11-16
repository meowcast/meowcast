/* RK322x SPI0 (spi-1 pins) W25Q128FV chunk writer, run as a MaskROM-helper callback.
 *
 * Makes flash[start, start+length) equal to DRAM[IMAGE_BASE+start, ...).
 * Flash bus commands: 0x9f ID, 0x05 read SR1, 0x03 read, and only in WRITE mode
 * 0x06 write-enable, 0x20 4 KiB sector erase, 0xd8 64 KiB block erase, 0x02 page program,
 * 0x04 write-disable. Every erase/program address is re-checked against the window.
 *
 * Speed (13 Nov): a 64 KiB block where >= BLOCK_ERASE_MIN sectors need erasing gets one
 * block erase (~150 ms) instead of up to 16 sector erases (~45 ms each), then every
 * sector of that block is programmed from the image. Verification is one pass: the
 * final CRC read of the whole chunk (FLAG_VERIFY_SECTORS adds the old per-sector
 * read-back). The SPI clock divider comes from the control block (even, 2..64).
 * Never: status/config register writes, chip/block erase, security registers,
 * 4-byte mode, reset, suspend. Compiled-in window [FLOOR, CEIL): the RKFP table,
 * vendor storage and both IDBlocks (< 0x50000) and misc/backup table (>= 0xfe0000)
 * cannot be written by this code whatever the control block says.
 *
 * Control block at 0x62000080 (written by the host before each call):
 *   [0] 'NWCB' [1] start [2] length [3] crc32(DRAM chunk) [4] mode [5] SPI divider
 *   [6] flags [7] ~(xor of 0..6)
 *   mode MODE_WRITE writes; anything else is a dry run (reads and compares only).
 * Result at 0x62000000 (32 words, see OUT_* below). SPI/pin state saved and restored
 * exactly as in the reviewed reader (boards/rk3228a-dongle/rockusb/spi_capture_impl.h). */
typedef unsigned int u32;
typedef unsigned char u8;
#define SPI 0x11090000u
#define GRF 0x11000000u
#define CRU 0x110e0000u
#define IMAGE_BASE 0x64000000u
#define FLOOR 0x00050000u
#define CEIL 0x00fe0000u
#define MAX_CHUNK 0x00040000u
#define SECTOR 4096u
#define BLOCK 65536u
#define BLOCK_ERASE_MIN 4u
#define FLAG_VERIFY_SECTORS 1u
#define PAGE 256u
#define CB_MAGIC 0x4243574eu   /* "NWCB" */
#define MODE_WRITE 0x54495257u /* "WRIT" */
#define OKAY 0x59414b4fu
#define BUSY_BUDGET 20000000u  /* >> 400 ms max 4 KiB erase at a few us per poll */

enum { E_CONFIG=1, E_SPI_BUSY=2, E_ID=3, E_CB=6, E_BOUNDS=7, E_RAM_CRC=8, E_SR1=9,
       E_WEL=10, E_TIMEOUT=11, E_VERIFY=12, E_XFER=13, E_FINAL_CRC=14 };
enum { OUT_MAGIC=0, OUT_STATUS=1, OUT_PROGRESS=2, OUT_XFER=3, OUT_SAVED=4,
       OUT_EQUAL=12, OUT_ERASED=13, OUT_PAGES=14, OUT_DIFFER=15, OUT_RAM_CRC=16,
       OUT_FLASH_CRC=17, OUT_SR1=18, OUT_SCTLR=19, OUT_ID=20, OUT_FAIL=21,
       OUT_MODE=22, OUT_START=23, OUT_LENGTH=24, OUT_RESTORED=26,
       OUT_FIFO=25, OUT_BLOCKS=29, OUT_DIV=30, OUT_FLAGS=31 };

static u32 rd(u32 a) { return *(volatile u32 *)a; }
static void wr(u32 a,u32 v) { *(volatile u32 *)a=v; }

/* FIFO depth, probed like Linux spi-rockchip (TXFTLR accepts values < depth); at most 32
 * bytes are kept in flight, so neither the TX nor the RX FIFO can overflow. */
static u32 fifo;

static u32 probe_fifo(void) {
    u32 n;
    for(n=2;n<32;n++) {
        wr(SPI+0x14,n);
        if(rd(SPI+0x14)!=n)break;
    }
    wr(SPI+0x14,0);
    return n;
}

/* One chip-select frame, full duplex PIO with bounded waits (reader's transfer(), plus an
 * optional transmit buffer for page program). Burst version (13 Nov): refill TX up to the
 * in-flight limit, then drain RXFLR entries, instead of one status read per byte. */
static int xfer(const u8 *header,u32 header_len,const volatile u8 *tx,volatile u8 *dest,u32 count) {
    const u32 total=header_len+count;
    if(total==0 || total>header_len+PAGE || fifo<2)return 1;
    wr(SPI+8,0);wr(SPI+4,total-1);wr(SPI+12,1);wr(SPI+8,1);
    u32 sent=0,got=0,budget=1000000;
    while(got<total) {
        u32 progress=0;
        while(sent<total && sent-got<fifo) {
            wr(SPI+0x400,sent<header_len?header[sent]:(tx?tx[sent-header_len]:0));
            sent++;progress=1;
        }
        u32 level=rd(SPI+0x20)&0x7f; /* RXFLR */
        if(level>sent-got)level=sent-got;
        for(;level;level--) {
            u32 value=rd(SPI+0x800);
            if(dest && got>=header_len)dest[got-header_len]=(u8)value;
            got++;progress=1;
        }
        if(progress)budget=1000000;
        else if(!--budget) {wr(SPI+12,0);wr(SPI+8,0);return 2;}
    }
    budget=1000000;
    while((rd(SPI+0x24)&1) && --budget){}
    wr(SPI+12,0);wr(SPI+8,0);
    return budget?0:3;
}

static int read_sr1(u8 *value) {
    const u8 op=0x05;
    return xfer(&op,1,0,(volatile u8 *)value,1);
}

static int read_flash(u32 address,u8 *dest) {
    const u8 header[4]={0x03,(u8)(address>>16),(u8)(address>>8),(u8)address};
    return xfer(header,4,0,(volatile u8 *)dest,PAGE);
}

static int wait_ready(void) {
    u8 sr=0xff;
    for(u32 budget=BUSY_BUDGET;budget;budget--) {
        if(read_sr1(&sr))return E_XFER;
        if(!(sr&1))return (sr&2)?E_WEL:0; /* WEL must have auto-cleared */
    }
    return E_TIMEOUT;
}

static int write_enable(void) {
    const u8 op=0x06;
    u8 sr=0;
    if(xfer(&op,1,0,0,0) || read_sr1(&sr))return E_XFER;
    return ((sr&3)==2)?0:E_WEL;
}

static void write_disable(void) {
    const u8 op=0x04;
    xfer(&op,1,0,0,0);
}

/* Bitwise CRC-32: in this MaskROM environment memory loads are expensive, so a lookup
 * table measured slower (13 Nov: 34.2 s vs 29.9 s for a dry run). */
static u32 crc32_update(u32 crc,const volatile u8 *p,u32 n) {
    for(u32 i=0;i<n;i++) {
        crc^=p[i];
        for(u32 k=0;k<8;k++)crc=(crc>>1)^(0xedb88320u&(0u-(crc&1)));
    }
    return crc;
}

/* Word-wide checks: callers pass 4-byte aligned buffers and n = PAGE. */
static int all_ff(const volatile u8 *p,u32 n) {
    const volatile u32 *w=(const volatile u32 *)p;
    for(u32 i=0;i<n/4;i++)if(w[i]!=0xffffffffu)return 0;
    return 1;
}

static int same(const u8 *a,const volatile u8 *b,u32 n) {
    const u32 *x=(const u32 *)a;
    const volatile u32 *y=(const volatile u32 *)b;
    for(u32 i=0;i<n/4;i++)if(x[i]!=y[i])return 0;
    return 1;
}

/* Compare one sector: returns 0 ok, sets *equal and *erased. */
static int compare_sector(u32 address,int *equal,int *erased) {
    u32 bufw[PAGE/4];u8 *buf=(u8 *)bufw;
    *equal=1;*erased=1;
    for(u32 o=0;o<SECTOR;o+=PAGE) {
        if(read_flash(address+o,buf))return E_XFER;
        if(!same(buf,(const volatile u8 *)(IMAGE_BASE+address+o),PAGE))*equal=0;
        if(!all_ff(buf,PAGE))*erased=0;
    }
    return 0;
}

/* Last line of defence: no erase/program command outside [FLOOR, CEIL). */
static int in_window(u32 address,u32 size) {
    return address>=FLOOR && address<CEIL && size<=CEIL-address;
}

static int erase(u8 op,u32 address,u32 size) {
    if(!in_window(address,size) || (address&(size-1)))return E_BOUNDS;
    const u8 header[4]={op,(u8)(address>>16),(u8)(address>>8),(u8)address};
    int error;
    if((error=write_enable()))return error;
    if(xfer(header,4,0,0,0))return E_XFER;
    return wait_ready();
}

/* Program every page of the sector that is not all 0xff in the image (sector erased). */
static int program_sector(volatile u32 *out,u32 address,int write) {
    if(!in_window(address,SECTOR))return E_BOUNDS;
    for(u32 o=0;o<SECTOR;o+=PAGE) {
        const volatile u8 *src=(const volatile u8 *)(IMAGE_BASE+address+o);
        if(all_ff(src,PAGE))continue;
        out[OUT_PAGES]++;
        if(write) {
            const u32 a=address+o;
            const u8 header[4]={0x02,(u8)(a>>16),(u8)(a>>8),(u8)a};
            int error;
            if((error=write_enable()))return error;
            if(xfer(header,4,src,0,PAGE))return E_XFER;
            if((error=wait_ready()))return error;
        }
    }
    return 0;
}

static int verify_sector(volatile u32 *out,u32 address) {
    int equal,erased,error;
    if((error=compare_sector(address,&equal,&erased)))return error;
    if(!equal) {out[OUT_FAIL]=address;return E_VERIFY;}
    return 0;
}

static int process(volatile u32 *out,u32 start,u32 length,int write,int verify) {
    const u32 end=start+length;
    for(u32 base=start;base<end;) {
        const u32 span=(!(base&(BLOCK-1)) && end-base>=BLOCK)?BLOCK:SECTOR;
        const u32 n=span/SECTOR;
        u32 differ=0,need=0,count=0;
        out[OUT_PROGRESS]=base;
        for(u32 i=0;i<n;i++) {
            int equal,erased,error;
            if((error=compare_sector(base+i*SECTOR,&equal,&erased)))return error;
            if(equal) {out[OUT_EQUAL]++;continue;}
            out[OUT_DIFFER]++;
            differ|=1u<<i;
            if(!erased) {need|=1u<<i;count++;}
        }
        if(span==BLOCK && count>=BLOCK_ERASE_MIN) {
            /* One block erase, then the whole block is programmed from the image. */
            out[OUT_BLOCKS]++;out[OUT_ERASED]+=n;
            if(write) {int e=erase(0xd8,base,BLOCK);if(e)return e;}
            for(u32 i=0;i<n;i++) {
                int e=program_sector(out,base+i*SECTOR,write);
                if(e)return e;
                if(write && verify && (e=verify_sector(out,base+i*SECTOR)))return e;
            }
        } else {
            for(u32 i=0;i<n;i++) {
                if(!(differ&(1u<<i)))continue;
                const u32 a=base+i*SECTOR;
                int e;
                if(need&(1u<<i)) {
                    out[OUT_ERASED]++;
                    if(write && (e=erase(0x20,a,SECTOR)))return e;
                }
                if((e=program_sector(out,a,write)))return e;
                if(write && verify && (e=verify_sector(out,a)))return e;
            }
        }
        base+=span;
    }
    out[OUT_PROGRESS]=end;
    return 0;
}

int stage_main(u32 opcode,u32 subcode,void *buffer,u32 length_unused) {
    (void)opcode;(void)subcode;(void)buffer;(void)length_unused;
    volatile u32 *out=(volatile u32 *)0x62000000;
    const volatile u32 *cb=(const volatile u32 *)0x62000080;
    for(u32 i=1;i<32;i++)out[i]=0;
    out[OUT_MAGIC]=0x57524f4e; /* NORW */
    out[OUT_FAIL]=0xffffffffu;
    u32 sctlr;
    __asm__ volatile("mrc p15, 0, %0, c1, c0, 0" : "=r"(sctlr));
    out[OUT_SCTLR]=sctlr;

    const u32 magic=cb[0],start=cb[1],length=cb[2],crc=cb[3],mode=cb[4],div=cb[5],flags=cb[6],check=cb[7];
    out[OUT_MODE]=mode;out[OUT_START]=start;out[OUT_LENGTH]=length;out[OUT_DIV]=div;out[OUT_FLAGS]=flags;
    if(magic!=CB_MAGIC || check!=~(magic^start^length^crc^mode^div^flags) ||
       div<2 || div>64 || (div&1) || (flags&~FLAG_VERIFY_SECTORS)) {out[OUT_STATUS]=E_CB;return 0;}
    if((start|length)&(SECTOR-1) || !length || length>MAX_CHUNK ||
       start<FLOOR || start>CEIL || length>CEIL-start) {out[OUT_STATUS]=E_BOUNDS;return 0;}
    const u32 ram_crc=~crc32_update(0xffffffffu,(const volatile u8 *)(IMAGE_BASE+start),length);
    out[OUT_RAM_CRC]=ram_crc;
    if(ram_crc!=crc) {out[OUT_STATUS]=E_RAM_CRC;return 0;}
    const int write=(mode==MODE_WRITE);

    u32 pins=rd(GRF+0x20),route=rd(GRF+0x50),clock_pin=rd(GRF+8);
    out[4]=pins;out[5]=route;out[6]=clock_pin;
    if(!(route&(1u<<5)) || ((clock_pin>>14)&3)!=2 || (rd(CRU+0xd8)&(1u<<9)) || (rd(CRU+0xf4)&(1u<<6))) {
        out[OUT_STATUS]=E_CONFIG;return 0;
    }
    u32 ctrl0=rd(SPI),ctrl1=rd(SPI+4),enable=rd(SPI+8),select=rd(SPI+12);
    u32 baud=rd(SPI+16),imr=rd(SPI+0x2c),dma=rd(SPI+0x3c),txftlr=rd(SPI+0x14);
    out[7]=ctrl0;out[8]=ctrl1;out[9]=enable;out[10]=select;out[11]=baud;
    if(rd(SPI+0x24)&1) {out[OUT_STATUS]=E_SPI_BUSY;return 0;}
    wr(SPI+8,0);wr(SPI+12,0);
    wr(GRF+0x20,(0x3fu<<16)|0x2a);
    wr(SPI,0x2401);wr(SPI+16,div);wr(SPI+0x2c,0);wr(SPI+0x3c,0);
    fifo=probe_fifo();
    out[OUT_FIFO]=fifo;

    int error=0;
    const u8 id_op=0x9f;
    volatile u8 *id=(volatile u8 *)(out+OUT_ID);
    u8 sr1=0xff;
    if(xfer(&id_op,1,0,id,3)) error=E_XFER;
    else if(id[0]!=0xef || id[1]!=0x40 || id[2]!=0x18) error=E_ID;
    else if(read_sr1(&sr1)) error=E_XFER;
    else {
        out[OUT_SR1]=sr1;
        if(sr1!=0) error=E_SR1; /* not busy, WEL clear, no block protection */
        else error=process(out,start,length,write,(flags&FLAG_VERIFY_SECTORS)!=0);
        if(write && error && error!=E_SR1) write_disable();
        if(!error) {
            /* Independent end-to-end check of the final flash contents. */
            u32 bufw[PAGE/4];u8 *buf=(u8 *)bufw;
            u32 c=0xffffffffu;
            for(u32 o=0;o<length && !error;o+=PAGE) {
                if(read_flash(start+o,buf)) error=E_XFER;
                else c=crc32_update(c,buf,PAGE);
            }
            out[OUT_FLASH_CRC]=~c;
            if(!error && ~c!=crc && write) error=E_FINAL_CRC;
        }
    }
    out[OUT_STATUS]=error?(u32)error:OKAY;

    wr(SPI+12,0);wr(SPI+8,0);
    wr(SPI,ctrl0);wr(SPI+4,ctrl1);wr(SPI+16,baud);
    wr(SPI+0x2c,imr);wr(SPI+0x3c,dma);wr(SPI+0x14,txftlr);
    wr(GRF+0x20,(0x3fu<<16)|(pins&0x3f));
    wr(SPI+12,select);wr(SPI+8,enable);
    out[OUT_RESTORED]=rd(GRF+0x20);out[27]=rd(SPI);out[28]=rd(SPI+8);
    __asm__ volatile("dsb sy" ::: "memory");
    return 0;
}
