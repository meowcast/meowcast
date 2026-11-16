/* Detect stray DMA writes into memory the GPU must not touch.
 * canary MB command [args...]
 * Fills MB MiB of anonymous memory with a per-word address-derived pattern,
 * runs the command (fork/exec, waits), then verifies every page and prints
 * each corrupted page's offset, damaged byte count and first words. The pages
 * stay allocated for the whole run, so the kernel cannot hand them to the GPU;
 * any change therefore comes from an out-of-bounds or stale-mapping write. */
#define _GNU_SOURCE
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

static uint32_t pattern(size_t i) { return (uint32_t)(i*2654435761u) ^ 0xa5c3e1f7u; }

int main(int argc,char **argv) {
    if(argc<3) { fprintf(stderr,"usage: canary MB command [args...]\n");return 2; }
    size_t mb=strtoul(argv[1],NULL,10),words=mb<<18;
    if(mb<1||mb>160) return 2;
    uint32_t *m=mmap(NULL,mb<<20,PROT_READ|PROT_WRITE,MAP_PRIVATE|MAP_ANONYMOUS|MAP_POPULATE,-1,0);
    if(m==MAP_FAILED) { perror("mmap");return 1; }
    if(mlock(m,mb<<20)) perror("mlock (continuing)");
    for(size_t i=0;i<words;i++)m[i]=pattern(i);
    printf("CANARY armed %zu MiB\n",mb);fflush(stdout);
    pid_t p=fork();
    if(!p) { execvp(argv[2],argv+2);_exit(127); }
    int st=0;waitpid(p,&st,0);
    size_t bad_pages=0;
    for(size_t pg=0;pg<(mb<<8);pg++) {
        size_t bad=0,first=0;
        for(size_t w=0;w<1024;w++) { size_t i=pg*1024+w; if(m[i]!=pattern(i)) { if(!bad)first=w;bad++; } }
        if(bad) {
            if(bad_pages<16) {
                printf("CANARY_HIT page=%zu bad_words=%zu first_word=%zu data=",pg,bad,first);
                for(size_t w=first;w<first+6 && w<1024;w++)printf("%08x ",m[pg*1024+w]);
                printf("\n");
            }
            bad_pages++;
        }
    }
    printf("CANARY result child_status=%d corrupted_pages=%zu %s\n",WIFEXITED(st)?WEXITSTATUS(st):-WTERMSIG(st),bad_pages,bad_pages?"CANARY_FAIL":"CANARY_PASS");
    return bad_pages?1:0;
}
