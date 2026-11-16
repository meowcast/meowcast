// memtest MiB SECONDS [THREADS]: DRAM/CPU error checker for voltage and DDR
// power-saving experiments. Each thread owns MiB/THREADS of memory and loops:
// fill with a pattern (address-derived xorshift, inverted on odd passes, plus
// walking ones), read back and verify. Prints one MEMTEST line; any error
// count > 0 means the setting under test is not safe.
#define _GNU_SOURCE
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <time.h>

struct job { uint32_t *buf; size_t words; double end; unsigned long passes, errors; int id; };

static double now(void)
{
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return ts.tv_sec + ts.tv_nsec / 1e9;
}

static inline uint32_t mix(uint32_t x)
{
	x ^= x << 13; x ^= x >> 17; x ^= x << 5;
	return x;
}

static void *run(void *arg)
{
	struct job *j = arg;
	unsigned long pass = 0;

	while (now() < j->end) {
		uint32_t seed = 0x9e3779b9u * (uint32_t)(pass + 1) + (uint32_t)j->id;
		uint32_t inv = (pass & 1) ? 0xffffffffu : 0;
		size_t i;

		if (pass % 3 == 2) {	/* walking ones */
			for (i = 0; i < j->words; i++)
				j->buf[i] = 1u << ((i + pass) & 31);
			for (i = 0; i < j->words; i++)
				if (j->buf[i] != 1u << ((i + pass) & 31)) {
					if (j->errors++ < 8)
						fprintf(stderr, "MEMTEST_ERR t%d word %zu got %08x want %08x\n", j->id, i,
							j->buf[i], 1u << ((i + pass) & 31));
				}
		} else {
			for (i = 0; i < j->words; i++)
				j->buf[i] = mix(seed ^ (uint32_t)i) ^ inv;
			for (i = 0; i < j->words; i++) {
				uint32_t want = mix(seed ^ (uint32_t)i) ^ inv;
				if (j->buf[i] != want && j->errors++ < 8)
					fprintf(stderr, "MEMTEST_ERR t%d word %zu got %08x want %08x\n", j->id, i,
						j->buf[i], want);
			}
		}
		pass++;
	}
	j->passes = pass;
	return NULL;
}

int main(int argc, char **argv)
{
	size_t mib = argc > 1 ? strtoul(argv[1], 0, 0) : 64;
	double secs = argc > 2 ? atof(argv[2]) : 60;
	int n = argc > 3 ? atoi(argv[3]) : 4, i;
	struct job jobs[16];
	pthread_t th[16];
	unsigned long passes = 0, errors = 0;
	double t0 = now();

	if (n < 1 || n > 16)
		return 2;
	for (i = 0; i < n; i++) {
		size_t bytes = (mib << 20) / n;
		uint32_t *b = mmap(0, bytes, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_POPULATE, -1, 0);

		if (b == MAP_FAILED) {
			perror("mmap");
			return 2;
		}
		jobs[i] = (struct job){ b, bytes / 4, t0 + secs, 0, 0, i };
		pthread_create(&th[i], 0, run, &jobs[i]);
	}
	for (i = 0; i < n; i++) {
		pthread_join(th[i], 0);
		passes += jobs[i].passes;
		errors += jobs[i].errors;
	}
	printf("MEMTEST mib=%zu threads=%d seconds=%.0f passes=%lu errors=%lu MB/s=%.0f\n", mib, n, now() - t0,
	       passes, errors, passes * 2.0 * mib / n / (now() - t0));
	return errors ? 1 : 0;
}
