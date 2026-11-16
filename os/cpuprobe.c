/* Separate CPU core speed from memory/cache behaviour.
 * alu: 200M iterations of a 4-instruction dependent-free loop body.
 * l1:  repeated 4 KiB array reads (always L1-resident when D-cache works). */
#define _GNU_SOURCE
#include <stdint.h>
#include <stdio.h>
#include <time.h>

static double now(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC,&t); return t.tv_sec+t.tv_nsec/1e9; }
static uint32_t buf[1024];

int main(void) {
    uint32_t n=50000000,a=1,b=2,c=3;
    double t=now();
    __asm__ volatile("1: add %1,%1,#1\n add %2,%2,#1\n add %3,%3,#1\n subs %0,%0,#1\n bne 1b"
                     : "+r"(n),"+r"(a),"+r"(b),"+r"(c) :: "cc");
    t=now()-t;
    /* 5 instructions/iteration; A7 issues at most 2/cycle, typically ~1.5-2.5 cycles here. */
    printf("CPUPROBE alu_loop=%.3fs iterations_per_us=%.1f (480 MHz A7 expect ~190-480)\n",t,50e6/t/1e6);
    for(int i=0;i<1024;i++)buf[i]=(uint32_t)i;
    uint32_t s=0;unsigned rounds=0;double start=now();
    do { for(int r=0;r<64;r++){const volatile uint32_t *p=buf;for(int i=0;i<1024;i+=4)s+=p[i]+p[i+1]+p[i+2]+p[i+3];} rounds+=64; } while((t=now())-start<0.5);
    printf("CPUPROBE l1_read=%.0f MB/s (healthy A7 at 480 MHz: roughly 700-1900)\n",rounds*4096.0/(t-start)/1e6);
    volatile uint32_t sink=s;(void)sink;
    return 0;
}
