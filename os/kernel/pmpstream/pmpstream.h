/* SPDX-License-Identifier: GPL-2.0 WITH Linux-syscall-note */
/*
 * pmpstream -- userspace interface to the sample DMA rings.
 *
 * /dev/pmp-tx and /dev/pmp-rx each expose a ring of equal-sized, physically
 * contiguous blocks, mmap()ed as one region (block i at offset i*block_bytes;
 * map nblocks*block_bytes rounded up to a whole page).
 * Samples are in the converter's native layout: interleaved I,Q little-endian
 * int16, 4 bytes per complex sample.
 *
 * The contract that matters: once started, the hardware is *always* fed.
 *   TX: if userspace has not committed a block in time, the kernel transmits a
 *       fill block instead and counts an underrun.  The stream never stops
 *       and never wedges.
 *   RX: if userspace has not consumed blocks in time, the kernel receives into
 *       a scratch block and counts an overrun.
 */
#ifndef PMPSTREAM_H
#define PMPSTREAM_H

#include <linux/ioctl.h>
#include <linux/types.h>

#define PMP_FILL_ZERO   0	/* underrun transmits silence */
#define PMP_FILL_REPEAT 1	/* underrun repeats the last committed block */

struct pmp_setup {
	__u32 block_bytes;	/* multiple of 8, <= 4 MiB */
	__u32 nblocks;		/* 2..64 */
	__u32 fill;		/* PMP_FILL_*, TX only */
	__u32 depth;		/* transfers kept queued in the DMA, 1..8; 0 = 4.
				 * Lower = a committed block goes out sooner, and
				 * less completion latency is tolerated before the
				 * stream gaps.  (Rust driver; the C one ignores it.) */
};

struct pmp_xfer {
	__u32 index;		/* block to fill (TX) / block that is ready (RX) */
	__u32 bytes;		/* valid bytes in it */
};

struct pmp_stats {
	__u64 blocks;		/* real blocks through the hardware */
	__u64 misses;		/* TX underruns / RX overruns, in blocks */
	__u32 queued;		/* blocks committed but not yet on air / filled but not yet read */
	__u32 running;
};

/* Lay out the ring ((nblocks + 1) * block_bytes <= 40 MiB).  Must precede
 * mmap().  Stream must be stopped. */
#define PMP_IOC_SETUP  _IOW('p', 1, struct pmp_setup)
/* Start the hardware.  TX: commit some blocks first to pre-buffer. */
#define PMP_IOC_START  _IO('p', 2)
#define PMP_IOC_STOP   _IO('p', 3)
/* TX: commit the block being filled (block 0 at first, then whatever the last
 * COMMIT returned); sleeps until another block is free and returns its index.
 * Keep signals away from the calling thread: the commit takes effect before
 * the sleep, so an interrupted call (EINTR from the Rust driver; a transparent
 * restart, i.e. a second commit, from the C one) loses track of the ring. */
#define PMP_IOC_COMMIT _IOWR('p', 4, struct pmp_xfer)
/* RX: release the block returned by the previous NEXT (if any), sleep until
 * the next block is full, return it. */
#define PMP_IOC_NEXT   _IOWR('p', 5, struct pmp_xfer)
#define PMP_IOC_STATS  _IOR('p', 6, struct pmp_stats)

#endif
