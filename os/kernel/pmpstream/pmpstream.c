// SPDX-License-Identifier: GPL-2.0
/*
 * pmpstream -- sample DMA rings that cannot starve the converter.
 *
 * A dmaengine client (the DMA controller itself is mainline's axi-dmac).  See
 * pmpstream.h for the userspace contract.  Everything that must happen on
 * time happens in the DMA completion callback: the next block -- real if
 * userspace has one ready, fill/scratch if not -- is queued from there, so no
 * amount of userspace scheduling delay can leave the hardware idle.
 */
#include <linux/delay.h>
#include <linux/dma-mapping.h>
#include <linux/dmaengine.h>
#include <linux/fs.h>
#include <linux/io.h>
#include <linux/miscdevice.h>
#include <linux/mm.h>
#include <linux/module.h>
#include <linux/of.h>
#include <linux/platform_device.h>
#include <linux/slab.h>
#include <linux/uaccess.h>
#include <linux/wait.h>

#include "pmpstream.h"

/*
 * Transfers kept queued in the DMA.  axi-dmac starts the next one from its
 * hard interrupt handler as long as it has a next one, so this is how much
 * completion-callback (tasklet) latency is absorbed, in blocks, before the
 * core runs dry and the stream gaps.  (With 1 the core idles after every
 * block; that configuration crashed the kernel within one session, against
 * ten to twenty sessions at 2 -- the one solid lead on the open stability
 * problem, see docs/ARCHITECTURE.md.)
 */
#define PMP_DEPTH	4
#define PMP_MAX_BLOCK	(4u << 20)
#define PMP_MAX_BLOCKS	64
#define PMP_DRAIN_MS	50
#define PMP_DRAIN_TIMEOUT	(3 * HZ)
/*
 * Each direction's ring lives in one allocation made at probe and never
 * freed, so that a DMA write that outlives its session can only land on
 * samples.  Defensive: see "Open problems" in docs/ARCHITECTURE.md -- kernel
 * memory is being corrupted with RX sample data by a mechanism not yet
 * identified, and per-session allocations made that strictly worse.
 */
#define PMP_POOL_BYTES	(40u << 20)

struct pmp_stream {
	struct miscdevice misc;
	struct dma_chan *chan;
	struct device *dmadev;
	bool tx;

	unsigned long busy;		/* bit 0: open */
	struct mutex lock;		/* serialises ioctls */
	spinlock_t ring_lock;		/* counters below, vs. the DMA callback */
	wait_queue_head_t wait;

	/*
	 * The ring is one DMA-coherent allocation: nblocks blocks, then a spare.
	 * Coherent (uncached) rather than streaming-mapped pages on purpose:
	 * userspace fills blocks through its own mapping, and on a Cortex-A9
	 * (VIPT D-cache, 8 KiB ways, 4 KiB pages) a user alias and the kernel
	 * alias of one page can sit in different cache sets, so the DMA API's
	 * flush of the kernel alias does not reach what userspace wrote.
	 */
	u32 block_bytes, nblocks, fill;
	void *ring;			/* the pool; never NULL after probe */
	dma_addr_t ring_dma;
	bool configured, running, spare_valid;
	void __iomem *adc;		/* RX only: converter core, for pmp_rx_gate() */

	/*
	 * Free-running block counters; index = counter % nblocks.
	 *   TX: user <= commits; k_sub, k_done chase it.
	 *   RX: k_sub, k_done lead; user (blocks released) chases k_done.
	 */
	u32 user, k_sub, k_done;
	u32 inflight;
	bool rx_holding;		/* RX: userspace holds block `user` */
	u64 stat_blocks, stat_misses;
	u32 stat_idle_restarts;		/* RX re-armed too late: core ran dry mid-stream */
};

static void pmp_top_up(struct pmp_stream *s);

/*
 * HYPOTHESIS, NOT A FIX.  The idea: starting an RX transfer on an idle
 * axi_dmac while the ADC is streaming into it is what sends samples to the
 * wrong addresses, so gate the ADC channels off in the converter core for the
 * moment it takes to queue transfers from idle.  A soak with this in place
 * still killed the board after ~9 sessions, so at best it is incomplete.  It
 * is kept because it is harmless and makes the start of an RX stream clean;
 * do not read it as an explanation.  Only bit 0 (enable) of the two channel
 * control registers is touched; the rest of that core belongs to userspace.
 */
#define ADC_CHAN_CNTRL(ch)	(0x400 + (ch) * 0x40)
#define ADC_CHAN_ENABLE		BIT(0)

static void pmp_rx_gate(struct pmp_stream *s, bool open)
{
	int ch;

	if (!s->adc)
		return;
	for (ch = 0; ch < 2; ch++) {
		u32 v = readl(s->adc + ADC_CHAN_CNTRL(ch));

		writel(open ? v | ADC_CHAN_ENABLE : v & ~ADC_CHAN_ENABLE,
		       s->adc + ADC_CHAN_CNTRL(ch));
	}
	if (!open)
		udelay(100);	/* let what is already in the pipe fall out */
}

static void pmp_done(struct pmp_stream *s, bool real)
{
	unsigned long flags;

	spin_lock_irqsave(&s->ring_lock, flags);
	s->inflight--;
	if (real) {
		s->k_done++;
		s->stat_blocks++;
	}
	if (s->running)
		pmp_top_up(s);
	spin_unlock_irqrestore(&s->ring_lock, flags);
	wake_up(&s->wait);	/* COMMIT/NEXT sleepers, and pmp_stop()'s drain */
}

static void pmp_done_real(void *p) { pmp_done(p, true); }
static void pmp_done_spare(void *p) { pmp_done(p, false); }

static int pmp_submit(struct pmp_stream *s, u32 idx, bool real)
{
	struct dma_async_tx_descriptor *d;
	enum dma_transfer_direction dir = s->tx ? DMA_MEM_TO_DEV : DMA_DEV_TO_MEM;
	dma_addr_t addr = s->ring_dma + (dma_addr_t)idx * s->block_bytes;

	/* Checked at run time, not by inspection: nothing leaves this function
	 * for the hardware that is not wholly inside the pool. */
	if (WARN_ONCE(!s->block_bytes || idx > s->nblocks || addr < s->ring_dma ||
		      (u64)(addr - s->ring_dma) + s->block_bytes > PMP_POOL_BYTES,
		      "pmpstream: refusing DMA outside pool: idx %u addr %pad len %u\n",
		      idx, &addr, s->block_bytes))
		return -EFAULT;

	d = dmaengine_prep_slave_single(s->chan, addr, s->block_bytes, dir, DMA_PREP_INTERRUPT);
	if (!d)
		return -ENOMEM;
	d->callback = real ? pmp_done_real : pmp_done_spare;
	d->callback_param = s;
	if (dma_submit_error(dmaengine_submit(d)))
		return -EIO;
	s->inflight++;
	return 0;
}

/* Called with ring_lock held.  Keep PMP_DEPTH transfers queued, always. */
static void pmp_top_up(struct pmp_stream *s)
{
	bool from_idle = !s->tx && s->inflight == 0;

	if (from_idle) {
		if (s->k_sub)
			s->stat_idle_restarts++;
		pmp_rx_gate(s, false);
	}
	while (s->inflight < PMP_DEPTH) {
		bool real;
		u32 idx;

		if (s->tx)
			real = s->k_sub != s->user;
		else
			real = s->k_sub - s->user < s->nblocks;

		if (real) {
			idx = s->k_sub % s->nblocks;
		} else {
			idx = s->nblocks;			/* the spare */
			s->stat_misses++;
			/* A block-sized memcpy with interrupts off -- but only
			 * once per underrun episode, with PMP_DEPTH blocks of
			 * air time still queued ahead of it. */
			if (s->tx && s->fill == PMP_FILL_REPEAT && !s->spare_valid && s->k_sub) {
				memcpy(s->ring + (size_t)idx * s->block_bytes,
				       s->ring + (size_t)((s->k_sub - 1) % s->nblocks) * s->block_bytes,
				       s->block_bytes);
				s->spare_valid = true;
			}
		}
		if (pmp_submit(s, idx, real))
			break;
		if (real) {
			s->k_sub++;
			s->spare_valid = false;
		}
	}
	dma_async_issue_pending(s->chan);
	if (from_idle)
		pmp_rx_gate(s, true);
}

/*
 * Stop by draining, not by terminating: stop submitting and let the queued
 * transfers complete (at most PMP_DEPTH block times; the converters' clock
 * never stops).  dmaengine_terminate_*() disables the core mid-burst, which
 * is a rougher thing to do to it than we need; it remains as the fallback for
 * hardware that has really hung.  This did not cure the open stability
 * problem either (docs/ARCHITECTURE.md) -- it is simply the gentler option.
 */
static void pmp_stop(struct pmp_stream *s)
{
	unsigned long flags;
	bool was_running;

	spin_lock_irqsave(&s->ring_lock, flags);
	was_running = s->running;
	s->running = false;
	spin_unlock_irqrestore(&s->ring_lock, flags);
	if (!was_running)
		return;

	if (!wait_event_timeout(s->wait, READ_ONCE(s->inflight) == 0, PMP_DRAIN_TIMEOUT)) {
		dev_warn(s->misc.parent, "%s: DMA did not drain; terminating\n", s->misc.name);
		dmaengine_terminate_sync(s->chan);
		msleep(PMP_DRAIN_MS);
	}

	spin_lock_irqsave(&s->ring_lock, flags);
	if (!s->tx)
		pmp_rx_gate(s, false);
	s->inflight = 0;
	s->user = s->k_sub = s->k_done = 0;
	s->rx_holding = false;
	s->spare_valid = false;
	spin_unlock_irqrestore(&s->ring_lock, flags);
	wake_up_interruptible(&s->wait);
}

static void pmp_release_ring(struct pmp_stream *s)
{
	s->configured = false;
	s->nblocks = 0;
}

static int pmp_setup(struct pmp_stream *s, const struct pmp_setup *c)
{
	if (s->running || s->configured)
		return -EBUSY;
	/* 8: the DMA moves whole 64-bit bus words. */
	if (!c->block_bytes || c->block_bytes > PMP_MAX_BLOCK || (c->block_bytes & 7) ||
	    c->nblocks < 2 || c->nblocks > PMP_MAX_BLOCKS || c->fill > PMP_FILL_REPEAT ||
	    (u64)(c->nblocks + 1) * c->block_bytes > PMP_POOL_BYTES)
		return -EINVAL;

	memset(s->ring, 0, (size_t)(c->nblocks + 1) * c->block_bytes);
	s->block_bytes = c->block_bytes;
	s->nblocks = c->nblocks;
	s->fill = c->fill;
	s->configured = true;
	return 0;
}

static long pmp_ioctl(struct file *f, unsigned int cmd, unsigned long arg)
{
	struct pmp_stream *s = container_of(f->private_data, struct pmp_stream, misc);
	void __user *up = (void __user *)arg;
	struct pmp_setup setup;
	struct pmp_stats st;
	struct pmp_xfer x;
	unsigned long flags;
	long ret = 0;

	/*
	 * Never behind the mutex: a COMMIT or NEXT can sleep under it for as
	 * long as the hardware takes, and the whole point of STATS is that a
	 * monitor can ask "is it moving?" precisely when it is not.
	 */
	if (cmd == PMP_IOC_STATS) {
		spin_lock_irqsave(&s->ring_lock, flags);
		st.blocks = s->stat_blocks;
		st.misses = s->stat_misses;
		st.queued = s->tx ? s->user - s->k_done : s->k_done - s->user;
		st.running = s->running;
		spin_unlock_irqrestore(&s->ring_lock, flags);
		return copy_to_user(up, &st, sizeof(st)) ? -EFAULT : 0;
	}

	mutex_lock(&s->lock);
	switch (cmd) {
	case PMP_IOC_SETUP:
		if (copy_from_user(&setup, up, sizeof(setup)))
			ret = -EFAULT;
		else
			ret = pmp_setup(s, &setup);
		break;

	case PMP_IOC_START:
		if (!s->configured) {
			ret = -ENXIO;
			break;
		}
		spin_lock_irqsave(&s->ring_lock, flags);
		if (!s->running) {
			s->running = true;
			pmp_top_up(s);
		}
		spin_unlock_irqrestore(&s->ring_lock, flags);
		break;

	case PMP_IOC_STOP:
		pmp_stop(s);
		break;

	case PMP_IOC_COMMIT:
		if (!s->tx || !s->configured) {
			ret = -ENXIO;
			break;
		}
		spin_lock_irqsave(&s->ring_lock, flags);
		s->user++;
		spin_unlock_irqrestore(&s->ring_lock, flags);
		/* The next block is ours once the hardware is done with it.
		 * Before START nothing completes, so pre-buffering is bounded
		 * by the ring: refuse rather than sleep forever. */
		if (!s->running && s->user - s->k_done >= s->nblocks) {
			s->user--;
			ret = -ENOSPC;
			break;
		}
		ret = wait_event_interruptible(s->wait,
				!s->running || s->user - READ_ONCE(s->k_done) < s->nblocks);
		if (ret)
			break;
		x.index = s->user % s->nblocks;
		x.bytes = s->block_bytes;
		if (copy_to_user(up, &x, sizeof(x)))
			ret = -EFAULT;
		break;

	case PMP_IOC_NEXT:
		if (s->tx || !s->configured) {
			ret = -ENXIO;
			break;
		}
		spin_lock_irqsave(&s->ring_lock, flags);
		if (s->rx_holding) {
			s->user++;
			s->rx_holding = false;
		}
		spin_unlock_irqrestore(&s->ring_lock, flags);
		ret = wait_event_interruptible(s->wait,
				!s->running || READ_ONCE(s->k_done) != s->user);
		if (ret)
			break;
		if (!s->running) {
			ret = -EPIPE;
			break;
		}
		x.index = s->user % s->nblocks;
		x.bytes = s->block_bytes;
		s->rx_holding = true;
		if (copy_to_user(up, &x, sizeof(x)))
			ret = -EFAULT;
		break;

	default:
		ret = -ENOTTY;
	}
	mutex_unlock(&s->lock);
	return ret;
}

static int pmp_mmap(struct file *f, struct vm_area_struct *vma)
{
	struct pmp_stream *s = container_of(f->private_data, struct pmp_stream, misc);
	unsigned long len = vma->vm_end - vma->vm_start;
	int ret = 0;

	mutex_lock(&s->lock);
	if (!s->configured || vma->vm_pgoff || len != PAGE_ALIGN((unsigned long)s->block_bytes * s->nblocks)) {
		ret = -EINVAL;
		goto out;
	}
	/* The spare block past the end stays the kernel's. */
	ret = dma_mmap_coherent(s->dmadev, vma, s->ring, s->ring_dma, len);
out:
	mutex_unlock(&s->lock);
	return ret;
}

static int pmp_open(struct inode *inode, struct file *f)
{
	struct pmp_stream *s = container_of(f->private_data, struct pmp_stream, misc);

	if (test_and_set_bit(0, &s->busy))
		return -EBUSY;
	if (s->stat_idle_restarts)
		dev_info(s->misc.parent, "%s: %u mid-stream restarts from idle last session\n",
			 s->misc.name, s->stat_idle_restarts);
	s->stat_blocks = s->stat_misses = 0;
	s->stat_idle_restarts = 0;
	return 0;
}

static int pmp_release(struct inode *inode, struct file *f)
{
	struct pmp_stream *s = container_of(f->private_data, struct pmp_stream, misc);

	/* A dying daemon must not leave the DAC replaying its last block. */
	mutex_lock(&s->lock);
	pmp_stop(s);
	pmp_release_ring(s);
	mutex_unlock(&s->lock);
	clear_bit(0, &s->busy);
	return 0;
}

static const struct file_operations pmp_fops = {
	.owner		= THIS_MODULE,
	.open		= pmp_open,
	.release	= pmp_release,
	.unlocked_ioctl	= pmp_ioctl,
	.mmap		= pmp_mmap,
};

static struct pmp_stream *pmp_stream_create(struct device *dev, const char *name, bool tx)
{
	struct pmp_stream *s = devm_kzalloc(dev, sizeof(*s), GFP_KERNEL);
	int ret;

	if (!s)
		return ERR_PTR(-ENOMEM);
	s->tx = tx;
	mutex_init(&s->lock);
	spin_lock_init(&s->ring_lock);
	init_waitqueue_head(&s->wait);

	s->chan = dma_request_chan(dev, tx ? "tx" : "rx");
	if (IS_ERR(s->chan))
		return ERR_CAST(s->chan);
	s->dmadev = s->chan->device->dev;
	s->ring = dma_alloc_coherent(s->dmadev, PMP_POOL_BYTES, &s->ring_dma, GFP_KERNEL);
	if (!s->ring) {
		dma_release_channel(s->chan);
		return ERR_PTR(-ENOMEM);
	}

	if (!tx) {
		u32 base;

		if (!of_property_read_u32(dev->of_node, "pmp,adc-core", &base))
			s->adc = devm_ioremap(dev, base, 0x1000);
		if (!s->adc)
			dev_warn(dev, "no pmp,adc-core: RX starts are not gated\n");
		pmp_rx_gate(s, false);
	}

	dev_info(dev, "%s pool %pad + %u MiB\n", name, &s->ring_dma, PMP_POOL_BYTES >> 20);

	s->misc.minor = MISC_DYNAMIC_MINOR;
	s->misc.name = name;
	s->misc.fops = &pmp_fops;
	s->misc.parent = dev;
	ret = misc_register(&s->misc);
	if (ret) {
		dma_free_coherent(s->dmadev, PMP_POOL_BYTES, s->ring, s->ring_dma);
		dma_release_channel(s->chan);
		return ERR_PTR(ret);
	}
	return s;
}

static void pmp_stream_destroy(struct pmp_stream *s)
{
	misc_deregister(&s->misc);
	dmaengine_terminate_sync(s->chan);
	msleep(PMP_DRAIN_MS);
	dma_free_coherent(s->dmadev, PMP_POOL_BYTES, s->ring, s->ring_dma);
	dma_release_channel(s->chan);
}

struct pmp_dev {
	struct pmp_stream *tx, *rx;
};

static int pmp_probe(struct platform_device *pdev)
{
	struct pmp_dev *p = devm_kzalloc(&pdev->dev, sizeof(*p), GFP_KERNEL);

	if (!p)
		return -ENOMEM;
	p->tx = pmp_stream_create(&pdev->dev, "pmp-tx", true);
	if (IS_ERR(p->tx))
		return dev_err_probe(&pdev->dev, PTR_ERR(p->tx), "tx stream\n");
	p->rx = pmp_stream_create(&pdev->dev, "pmp-rx", false);
	if (IS_ERR(p->rx)) {
		pmp_stream_destroy(p->tx);
		return dev_err_probe(&pdev->dev, PTR_ERR(p->rx), "rx stream\n");
	}
	platform_set_drvdata(pdev, p);
	dev_info(&pdev->dev, "tx on %s, rx on %s\n",
		 dma_chan_name(p->tx->chan), dma_chan_name(p->rx->chan));
	return 0;
}

static void pmp_remove(struct platform_device *pdev)
{
	struct pmp_dev *p = platform_get_drvdata(pdev);

	pmp_stream_destroy(p->rx);
	pmp_stream_destroy(p->tx);
}

static const struct of_device_id pmp_of_match[] = {
	{ .compatible = "pmp,stream" },
	{ }
};
MODULE_DEVICE_TABLE(of, pmp_of_match);

static struct platform_driver pmp_driver = {
	.probe = pmp_probe,
	.remove = pmp_remove,
	.driver = {
		.name = "pmpstream",
		.of_match_table = pmp_of_match,
	},
};
module_platform_driver(pmp_driver);

MODULE_DESCRIPTION("pimpmypluto sample DMA rings");
MODULE_LICENSE("GPL");
