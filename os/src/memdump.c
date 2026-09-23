/* memdump ADDR LEN -- write a range of physical memory to stdout.
 * mmap, not read(): /dev/mem cannot read() RAM the kernel has not mapped
 * (reserved-memory no-map regions such as the ramoops log). */
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <unistd.h>

int main(int argc, char **argv)
{
	unsigned long addr, len, off = 0;
	unsigned char *p;
	int fd;

	if (argc != 3) {
		fprintf(stderr, "usage: memdump ADDR LEN\n");
		return 2;
	}
	addr = strtoul(argv[1], NULL, 0);
	len = strtoul(argv[2], NULL, 0);
	fd = open("/dev/mem", O_RDONLY | O_SYNC);
	if (fd < 0) {
		perror("/dev/mem");
		return 1;
	}
	p = mmap(NULL, len, PROT_READ, MAP_SHARED, fd, (off_t)addr);
	if (p == MAP_FAILED) {
		perror("mmap");
		return 1;
	}
	while (off < len) {
		ssize_t n = write(1, p + off, len - off);
		if (n <= 0)
			return 1;
		off += (unsigned long)n;
	}
	return 0;
}
