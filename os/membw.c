/* CPU-side memory probe. Reads at L1 (16 KiB), L2 (192 KiB) and DRAM (16 MiB)
 * working-set sizes, DRAM write/copy, and DRAM load-to-use latency by pointer
 * chase. One Cortex-A7 cannot saturate the DDR controller; use this to see
 * whether the CPU memory path is healthy and to compare DDR settings. */
#define _GNU_SOURCE
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#define BIG (16u<<20)
static double now(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC,&t); return t.tv_sec+t.tv_nsec/1e9; }
#define BARRIER(p) __asm__ volatile("" :: "r"(p) : "memory")

static double read_mbs(const uint32_t *a,size_t bytes) {
    double start=now(),t;size_t total=0;uint32_t s=0;
    do { for(size_t i=0;i<bytes/4;i+=4)s+=a[i]^a[i+1]^a[i+2]^a[i+3];total+=bytes;BARRIER(a); } while((t=now())-start<0.5);
    volatile uint32_t sink=s;(void)sink;return total/(t-start)/1e6;
}

int main(void) {
    uint32_t *a=aligned_alloc(64,BIG),*b=aligned_alloc(64,BIG);
    if(!a||!b) { puts("MEMBW_FAIL allocation");return 1; }
    memset(a,1,BIG);memset(b,2,BIG);
    printf("MEMBW read_l1=%.0f read_l2=%.0f read_dram=%.0f MB/s\n",read_mbs(a,16384),read_mbs(a,196608),read_mbs(a,BIG));
    double t,start;size_t total=0;
    for(start=now();(t=now())-start<0.5;total+=BIG) { memset(a,(int)total,BIG);BARRIER(a); }
    printf("MEMBW write_dram=%.0f MB/s\n",total/(t-start)/1e6);
    for(total=0,start=now();(t=now())-start<0.5;total+=BIG) { memcpy(b,a,BIG);BARRIER(b); }
    printf("MEMBW copy_dram=%.0f MB/s (bus traffic read+write %.0f MB/s)\n",total/(t-start)/1e6,2.0*total/(t-start)/1e6);
    /* Random cyclic permutation over 64-byte lines defeats the prefetcher. */
    size_t lines=BIG/64,step=16;uint32_t *perm=(uint32_t*)b;
    for(size_t i=0;i<lines;i++)perm[i]=(uint32_t)i;
    srand(1);for(size_t i=lines-1;i>0;i--){size_t j=(size_t)rand()%(i+1);uint32_t x=perm[i];perm[i]=perm[j];perm[j]=x;}
    for(size_t i=0;i<lines;i++)a[perm[i]*step]=perm[(i+1)%lines]*step;
    uint32_t p=perm[0]*step;size_t hops=0;
    for(start=now();(t=now())-start<0.5;hops+=4096) for(int k=0;k<4096;k++)p=a[p];
    volatile uint32_t sink=p;(void)sink;
    printf("MEMBW latency_dram=%.0f ns\n",(t-start)/hops*1e9);
    free(a);free(b);return 0;
}
