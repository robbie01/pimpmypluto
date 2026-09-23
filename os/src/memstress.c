/* memstress [threads] [MiB per thread | NNNk for KiB] [seconds]
 * Fill large buffers with a pseudo-random pattern and verify them, from several
 * threads at once.  Any mismatch is memory that did not hold what was written:
 * marginal DDR timing, a cache-coherency fault, or something DMA-writing where
 * it should not.  Reports address (virtual), expected and found. */
#define _GNU_SOURCE
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

static size_t words;
static int seconds;
static volatile unsigned long errors, passes;

#include <fcntl.h>
#include <unistd.h>
/* virtual -> physical, via /proc/self/pagemap (root only) */
static uint64_t phys_of(void *v)
{
	uint64_t e = 0;
	int fd = open("/proc/self/pagemap", O_RDONLY);
	if (fd < 0) return 0;
	if (pread(fd, &e, 8, ((uintptr_t)v >> 12) * 8) != 8) e = 0;
	close(fd);
	return (e >> 63) ? ((e & ((1ULL << 55) - 1)) << 12) | ((uintptr_t)v & 0xfff) : 0;
}

static inline uint32_t next(uint32_t *s) { *s ^= *s << 13; *s ^= *s >> 17; *s ^= *s << 5; return *s; }

static void *worker(void *arg)
{
	long id = (long)arg;
	uint32_t *buf = malloc(words * 4), seed0 = 0x9e3779b9u * (uint32_t)(id + 1);
	time_t end = time(NULL) + seconds;
	if (!buf) { perror("malloc"); return NULL; }
	while (time(NULL) < end) {
		uint32_t prev_seed = seed0, s = seed0 += 0x7f4a7c15u, ps = prev_seed;
		size_t i;
		for (i = 0; i < words; i++) buf[i] = next(&s);
		s = seed0;
		for (i = 0; i < words; i++) {
			uint32_t want = next(&s), old = next(&ps);	/* old: what the previous pass wrote here */
			if (buf[i] != want) {
				if (__sync_add_and_fetch(&errors, 1) <= 40 && (i & 7) == ((uintptr_t)buf >> 2 & 0 ) + (i & 7))
					fprintf(stderr, "memstress: cpu-thread %ld phys %08llx (virt %p, line offset %02x): %s\n",
						id, (unsigned long long)phys_of(&buf[i]), (void *)&buf[i],
						(unsigned)((uintptr_t)&buf[i] & 0x1f),
						buf[i] == old ? "holds PREVIOUS pass value (write lost)" : "holds something else");
			}
		}
		__sync_add_and_fetch(&passes, 1);
	}
	return NULL;
}

int main(int argc, char **argv)
{
	int n = argc > 1 ? atoi(argv[1]) : 2, mib = argc > 2 ? atoi(argv[2]) : 96, i;
	pthread_t t[16];
	seconds = argc > 3 ? atoi(argv[3]) : 120;
	words = (size_t)mib << 18;
	if (argc > 2 && strchr(argv[2], 'k'))		/* small enough to live in cache */
		words = (size_t)mib << 8;
	for (i = 0; i < n && i < 16; i++) pthread_create(&t[i], NULL, worker, (void *)(long)i);
	for (i = 0; i < n && i < 16; i++) pthread_join(t[i], NULL);
	fprintf(stderr, "memstress: %d threads x %zu KiB, %d s: %lu passes, %lu errors\n", n, words / 256, seconds, passes, errors);
	return errors != 0;
}
