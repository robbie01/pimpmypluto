/* Force-included into the kernel's host tools when building on macOS. */
#pragma once
#ifdef __APPLE__
#include <sys/types.h>
#ifndef O_LARGEFILE
#define O_LARGEFILE 0
#endif
/* usr/gen_init_cpio.c: a failed copy_file_range() falls back to read/write. */
static inline ssize_t copy_file_range(int in, void *offin, int out, void *offout,
				      size_t len, unsigned int flags)
{
	(void)in; (void)offin; (void)out; (void)offout; (void)len; (void)flags;
	return -1;
}
#endif
