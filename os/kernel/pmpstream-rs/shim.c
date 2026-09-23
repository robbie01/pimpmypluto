// SPDX-License-Identifier: GPL-2.0
/*
 * The part of pmpstream that has to be C: dmaengine's client API is mostly
 * static inline functions, and Rust-for-Linux has no abstraction for it.  No
 * policy lives here -- every function is a direct spelling of one dmaengine
 * call.  The driver is pmpstream_core.rs.
 */
#include <linux/delay.h>
#include <linux/dmaengine.h>
#include <linux/err.h>

int pmp_shim_request(struct device *dev, const char *name,
		     struct dma_chan **chan, struct device **dmadev);
void pmp_shim_release(struct dma_chan *chan);
int pmp_shim_submit(struct dma_chan *chan, u64 addr, u32 len, bool to_dev,
		    void (*done)(void *), void *param);
void pmp_shim_issue_pending(struct dma_chan *chan);
void pmp_shim_terminate_sync(struct dma_chan *chan);
void pmp_shim_udelay(u32 us);

int pmp_shim_request(struct device *dev, const char *name,
		     struct dma_chan **chan, struct device **dmadev)
{
	struct dma_chan *c = dma_request_chan(dev, name);

	if (IS_ERR(c))
		return PTR_ERR(c);
	*chan = c;
	*dmadev = c->device->dev;
	return 0;
}

void pmp_shim_release(struct dma_chan *chan)
{
	dma_release_channel(chan);
}

int pmp_shim_submit(struct dma_chan *chan, u64 addr, u32 len, bool to_dev,
		    void (*done)(void *), void *param)
{
	struct dma_async_tx_descriptor *d;

	d = dmaengine_prep_slave_single(chan, addr, len,
					to_dev ? DMA_MEM_TO_DEV : DMA_DEV_TO_MEM,
					DMA_PREP_INTERRUPT);
	if (!d)
		return -ENOMEM;
	d->callback = done;
	d->callback_param = param;
	return dma_submit_error(dmaengine_submit(d)) ? -EIO : 0;
}

void pmp_shim_issue_pending(struct dma_chan *chan)
{
	dma_async_issue_pending(chan);
}

void pmp_shim_terminate_sync(struct dma_chan *chan)
{
	dmaengine_terminate_sync(chan);
}

void pmp_shim_udelay(u32 us)
{
	udelay(us);
}
