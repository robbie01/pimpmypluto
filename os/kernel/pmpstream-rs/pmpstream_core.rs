// SPDX-License-Identifier: GPL-2.0

//! pmpstream -- sample DMA rings that cannot starve the converter.
//!
//! A dmaengine client (the DMA controller itself is mainline's axi-dmac).  See
//! `../pmpstream/pmpstream.h` for the userspace contract; this is a port of
//! `../pmpstream/pmpstream.c` and keeps its behaviour.  Everything that must
//! happen on time happens in the DMA completion callback: the next block --
//! real if userspace has one ready, fill/scratch if not -- is queued from
//! there, so no amount of userspace scheduling delay can leave the hardware
//! idle.
//!
//! # Concurrency
//!
//! The C driver guards its ring counters with a `spin_lock_irqsave()`, which
//! the completion callback (a tasklet) and the ioctls both take.  The kernel's
//! Rust `SpinLock` has no irqsave or bh variant yet, so a callback arriving on
//! the CPU that holds it would deadlock.  Instead:
//!
//! * every counter is an atomic with exactly one writer at a time --
//!   `user` belongs to the ioctls (serialised by the `ctl` mutex), `k_done`
//!   and `inflight`'s decrement to the callback (a tasklet never runs
//!   concurrently with itself), `k_sub` to whoever holds `top_lock`;
//! * `top_lock` is a *try*-lock around queueing to the DMA.  A loser does not
//!   wait: it leaves `top_again` set and the holder goes round once more.  That
//!   is what makes it safe against a callback interrupting the holder;
//! * all of them reset only in `stop()`, after `running` is clear, `top_lock`
//!   has been passed through once (so no queueing is in progress or can start)
//!   and the hardware has drained (so no callback is left to come).

use core::{
    ffi::c_void,
    ptr,
    sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::SeqCst},
};
use kernel::{
    bindings, c_str, device,
    error::to_result,
    fs::File,
    ioctl::{_IO, _IOR, _IOW, _IOWR},
    miscdevice::{MiscDevice, MiscDeviceOptions, MiscDeviceRegistration},
    mm::virt::VmaNew,
    new_condvar, new_mutex, of,
    page::PAGE_SIZE,
    platform,
    prelude::*,
    str::CStr,
    sync::{Arc, ArcBorrow, CondVar, CondVarTimeoutResult, Mutex},
    time::{delay::fsleep, Delta},
    transmute::{AsBytes, FromBytes},
    uaccess::UserSlice,
};

// shim.c: dmaengine's client API is static inlines, which bindgen cannot see.
extern "C" {
    fn pmp_shim_request(
        dev: *mut bindings::device,
        name: *const kernel::ffi::c_char,
        chan: *mut *mut c_void,
        dmadev: *mut *mut bindings::device,
    ) -> kernel::ffi::c_int;
    fn pmp_shim_release(chan: *mut c_void);
    fn pmp_shim_submit(
        chan: *mut c_void,
        addr: u64,
        len: u32,
        to_dev: bool,
        done: unsafe extern "C" fn(*mut c_void),
        param: *mut c_void,
    ) -> kernel::ffi::c_int;
    fn pmp_shim_issue_pending(chan: *mut c_void);
    fn pmp_shim_terminate_sync(chan: *mut c_void);
    fn pmp_shim_udelay(us: u32);
}

/// Transfers kept queued in the DMA.  axi-dmac starts the next one from its
/// hard interrupt handler as long as it has a next one, so this is how much
/// completion-callback (tasklet) latency is absorbed, in blocks, before the
/// core runs dry and the stream gaps.
const DEPTH: u32 = 4;
/// ...unless `pmp_setup.depth` asks otherwise: a burst responder trades that
/// cushion for latency (a committed block goes out behind what is queued).
const MAX_DEPTH: u32 = 8;
const MAX_BLOCK: u32 = 4 << 20;
const MAX_BLOCKS: u32 = 64;
const DRAIN_SETTLE_MS: i64 = 50;
const DRAIN_TIMEOUT_MS: i64 = 3000;
/// Each direction's ring lives in one allocation made at probe and never freed
/// while the driver is bound, so that a DMA write that outlives its session
/// can only ever land on samples.
const POOL_BYTES: usize = 40 << 20;

const FILL_REPEAT: u32 = 1;

// The userspace ABI: pmpstream.h.
#[repr(C)]
#[derive(Clone, Copy)]
struct Setup {
    block_bytes: u32,
    nblocks: u32,
    fill: u32,
    depth: u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct Xfer {
    index: u32,
    bytes: u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct Stats {
    blocks: u64,
    misses: u64,
    queued: u32,
    running: u32,
}
// SAFETY: plain integers, no padding, every bit pattern valid.
unsafe impl FromBytes for Setup {}
// SAFETY: as above.
unsafe impl AsBytes for Xfer {}
// SAFETY: as above (8 + 8 + 4 + 4: no padding).
unsafe impl AsBytes for Stats {}

const IOC_SETUP: u32 = _IOW::<Setup>('p' as u32, 1);
const IOC_START: u32 = _IO('p' as u32, 2);
const IOC_STOP: u32 = _IO('p' as u32, 3);
const IOC_COMMIT: u32 = _IOWR::<Xfer>('p' as u32, 4);
const IOC_NEXT: u32 = _IOWR::<Xfer>('p' as u32, 5);
const IOC_STATS: u32 = _IOR::<Stats>('p' as u32, 6);

// axi_ad9361 ADC core, channel control registers; bit 0 is "enable".
const fn adc_chan_cntrl(ch: usize) -> usize {
    0x400 + ch * 0x40
}
const ADC_CHAN_ENABLE: u32 = 1;

/// `counter % nblocks` without a 64-bit division, which a 32-bit ARM kernel does
/// not link.  `nblocks` is at most `MAX_BLOCKS`, so nothing here overflows.
fn ring_index(counter: u64, nblocks: u32) -> usize {
    let two32 = (u32::MAX % nblocks + 1) % nblocks; // 2^32 mod nblocks
    let (hi, lo) = ((counter >> 32) as u32, counter as u32);
    ((hi % nblocks * two32 + lo % nblocks) % nblocks) as usize
}

/// State only the ioctls touch; the mutex around it also serialises them.
struct Ctl {
    /// RX: userspace holds block `user`.
    rx_holding: bool,
}

/// What `Stream::new` acquired; fixed for the stream's life.
struct Hw {
    tx: bool,
    name: &'static CStr,
    chan: *mut c_void,
    dmadev: *mut bindings::device,
    /// The pool: `nblocks` blocks, then a spare.  DMA-coherent (uncached)
    /// rather than streaming-mapped pages on purpose: userspace fills blocks
    /// through its own mapping, and on a Cortex-A9 (VIPT D-cache) a user alias
    /// and the kernel alias of one page can sit in different cache sets, so the
    /// DMA API's flush of the kernel alias does not reach what userspace wrote.
    ring: *mut u8,
    ring_dma: bindings::dma_addr_t,
    /// RX only, may be null: converter core, for `rx_gate()`.
    adc: *mut c_void,
}

impl Drop for Hw {
    fn drop(&mut self) {
        // SAFETY: all of these came from `Stream::new` and nothing else can
        // reach them any more.  Terminating first means no transfer and no
        // callback survives into the frees.  Process context: the last
        // reference goes in probe, in the driver's remove or in a file's release.
        unsafe {
            pmp_shim_terminate_sync(self.chan);
            fsleep(Delta::from_millis(DRAIN_SETTLE_MS));
            bindings::dma_free_attrs(self.dmadev, POOL_BYTES, self.ring.cast(), self.ring_dma, 0);
            pmp_shim_release(self.chan);
            if !self.adc.is_null() {
                bindings::iounmap(self.adc);
            }
        }
    }
}

/// The ring's state: all atomics, see "Concurrency" above.
#[derive(Default)]
struct Ring {
    busy: AtomicBool,

    // Written under `ctl` while stopped; read by everyone.
    block_bytes: AtomicU32,
    nblocks: AtomicU32,
    fill: AtomicU32,
    depth: AtomicU32,
    configured: AtomicBool,
    running: AtomicBool,

    // Free-running block counters; index = counter % nblocks.
    //   TX: user <= commits; k_sub, k_done chase it.
    //   RX: k_sub, k_done lead; user (blocks released) chases k_done.
    // 64-bit so that "% nblocks" never meets a wrap.
    user: AtomicU64,
    k_sub: AtomicU64,
    k_done: AtomicU64,
    inflight: AtomicU32,

    top_lock: AtomicBool,
    top_again: AtomicBool,
    spare_valid: AtomicBool,

    stat_blocks: AtomicU64,
    stat_misses: AtomicU64,
    /// RX re-armed too late: the core ran dry mid-stream.
    stat_idle_restarts: AtomicU32,
}

/// `hw` is declared, so dropped, first: once the channel is terminated no
/// callback can reach the rest.
#[pin_data]
struct Stream {
    hw: Hw,
    r: Ring,
    #[pin]
    ctl: Mutex<Ctl>,
    #[pin]
    wait: CondVar,
}

// SAFETY: the raw pointers are to a DMA channel, a device, a coherent buffer
// and an MMIO mapping, all owned by this object for its whole life and all
// usable from any context; everything mutable is atomic or behind `ctl`.
unsafe impl Send for Stream {}
// SAFETY: as above.
unsafe impl Sync for Stream {}

unsafe extern "C" fn done_real(p: *mut c_void) {
    // SAFETY: `p` is the `Stream` that submitted this transfer; `Hw::drop`
    // terminates the channel synchronously before the memory goes away.
    unsafe { &*p.cast::<Stream>() }.done(true)
}

unsafe extern "C" fn done_spare(p: *mut c_void) {
    // SAFETY: as in `done_real`.
    unsafe { &*p.cast::<Stream>() }.done(false)
}

impl Stream {
    fn new(dev: &device::Device<device::Core>, tx: bool, adc_base: Option<u32>) -> Result<Arc<Self>> {
        let name = if tx { c_str!("pmp-tx") } else { c_str!("pmp-rx") };
        let mut chan = ptr::null_mut();
        let mut dmadev = ptr::null_mut();
        // SAFETY: `Device` is a transparent wrapper of `struct device`; the
        // out-pointers are valid; the name is NUL-terminated.
        to_result(unsafe {
            pmp_shim_request(
                ptr::from_ref(dev).cast_mut().cast(),
                if tx { c_str!("tx") } else { c_str!("rx") }.as_char_ptr(),
                &mut chan,
                &mut dmadev,
            )
        })?;

        let mut ring_dma: bindings::dma_addr_t = 0;
        // SAFETY: `dmadev` is the DMA controller's device, kept alive by the
        // channel we hold.
        let ring = unsafe { bindings::dma_alloc_attrs(dmadev, POOL_BYTES, &mut ring_dma, bindings::GFP_KERNEL, 0) };
        if ring.is_null() {
            // SAFETY: `chan` came from `pmp_shim_request`.
            unsafe { pmp_shim_release(chan) };
            return Err(ENOMEM);
        }

        // A property, not a `reg`: the region itself belongs to userspace (UIO).
        let adc = match adc_base {
            // SAFETY: mapping a fixed MMIO region named by the device tree.
            Some(base) => unsafe { bindings::ioremap(base.into(), 0x1000) },
            None => ptr::null_mut(),
        };
        if !tx && adc.is_null() {
            dev_warn!(dev, "no pmp,adc-core: RX starts are not gated\n");
        }
        dev_info!(dev, "{} pool {:#x} + {} MiB\n", name, ring_dma, POOL_BYTES >> 20);

        // From here on `Hw::drop` releases everything, including on failure.
        let hw = Hw { tx, name, chan, dmadev, ring: ring.cast(), ring_dma, adc };
        let s = Arc::pin_init(
            pin_init!(Stream {
                hw,
                r: Ring::default(),
                ctl <- new_mutex!(Ctl { rx_holding: false }),
                wait <- new_condvar!(),
            }),
            GFP_KERNEL,
        )?;
        s.rx_gate(false);
        Ok(s)
    }

    /// HYPOTHESIS, NOT A FIX -- kept from the C driver, which was soak-tested
    /// with it: gate the ADC channels off in the converter core for the moment
    /// it takes to queue RX transfers from idle, so that a stream starts on a
    /// clean block boundary.  Only bit 0 of the two channel control registers
    /// is touched; the rest of that core belongs to userspace.
    fn rx_gate(&self, open: bool) {
        if self.hw.adc.is_null() {
            return;
        }
        for ch in 0..2 {
            // SAFETY: inside the 0x1000 bytes mapped in `new`.
            unsafe {
                let reg = self.hw.adc.byte_add(adc_chan_cntrl(ch));
                let v = bindings::readl(reg);
                bindings::writel(if open { v | ADC_CHAN_ENABLE } else { v & !ADC_CHAN_ENABLE }, reg);
            }
        }
        if !open {
            // Let what is already in the pipe fall out.
            // SAFETY: a delay.
            unsafe { pmp_shim_udelay(100) };
        }
    }

    /// DMA completion callback (tasklet context).
    fn done(&self, real: bool) {
        self.r.inflight.fetch_sub(1, SeqCst);
        if real {
            self.r.k_done.fetch_add(1, SeqCst);
            self.r.stat_blocks.fetch_add(1, SeqCst);
        }
        self.top_up();
        // COMMIT/NEXT sleepers, and `stop()`'s drain.
        self.wait.notify_all();
    }

    /// Keep `DEPTH` transfers queued, always.  Any context; never waits.
    fn top_up(&self) {
        loop {
            if self.r.top_lock.swap(true, SeqCst) {
                // Someone is at it.  Ask them for another round; if they were
                // still inside when we asked they will see it, otherwise go
                // round ourselves.
                self.r.top_again.store(true, SeqCst);
                if self.r.top_lock.load(SeqCst) {
                    return;
                }
                continue;
            }
            self.r.top_again.store(false, SeqCst);
            if self.r.running.load(SeqCst) {
                self.fill_queue();
            }
            self.r.top_lock.store(false, SeqCst);
            if !self.r.top_again.swap(false, SeqCst) {
                return;
            }
        }
    }

    /// Caller holds `top_lock` and has seen `running`.
    fn fill_queue(&self) {
        let nblocks = self.r.nblocks.load(SeqCst);
        let block_bytes = self.r.block_bytes.load(SeqCst) as usize;
        let from_idle = !self.hw.tx && self.r.inflight.load(SeqCst) == 0;

        if from_idle {
            if self.r.k_sub.load(SeqCst) != 0 {
                self.r.stat_idle_restarts.fetch_add(1, SeqCst);
            }
            self.rx_gate(false);
        }
        let depth = self.r.depth.load(SeqCst);
        while self.r.inflight.load(SeqCst) < depth {
            let k_sub = self.r.k_sub.load(SeqCst);
            let user = self.r.user.load(SeqCst);
            let real = if self.hw.tx { k_sub != user } else { k_sub.wrapping_sub(user) < u64::from(nblocks) };
            let idx = if real {
                ring_index(k_sub, nblocks)
            } else {
                self.r.stat_misses.fetch_add(1, SeqCst);
                // A block-sized copy in the callback -- but only once per
                // underrun episode, with DEPTH blocks of air time still queued
                // ahead of it.
                if self.hw.tx && self.r.fill.load(SeqCst) == FILL_REPEAT && k_sub != 0 && !self.r.spare_valid.swap(true, SeqCst) {
                    // SAFETY: both blocks are inside the pool (`setup` checked
                    // (nblocks + 1) * block_bytes) and distinct.
                    unsafe {
                        ptr::copy_nonoverlapping(
                            self.hw.ring.add(ring_index(k_sub - 1, nblocks) * block_bytes),
                            self.hw.ring.add(nblocks as usize * block_bytes),
                            block_bytes,
                        )
                    };
                }
                nblocks as usize // the spare
            };
            if self.submit(idx, block_bytes, real).is_err() {
                break;
            }
            if real {
                self.r.k_sub.store(k_sub + 1, SeqCst);
                self.r.spare_valid.store(false, SeqCst);
            }
        }
        // SAFETY: `chan` is ours.
        unsafe { pmp_shim_issue_pending(self.hw.chan) };
        if from_idle {
            self.rx_gate(true);
        }
    }

    fn submit(&self, idx: usize, block_bytes: usize, real: bool) -> Result {
        let offset = idx * block_bytes;
        // Checked at run time, not by inspection: nothing leaves this function
        // for the hardware that is not wholly inside the pool.
        if block_bytes == 0 || offset + block_bytes > POOL_BYTES {
            static WARNED: AtomicBool = AtomicBool::new(false);
            if !WARNED.swap(true, SeqCst) {
                pr_warn!("pmpstream: refusing DMA outside pool: idx {} len {}\n", idx, block_bytes);
            }
            return Err(EFAULT);
        }
        self.r.inflight.fetch_add(1, SeqCst);
        // SAFETY: the transfer is inside the pool, which outlives the channel's
        // activity (`drop` terminates first); the callback parameter is `self`,
        // likewise.
        let ret = to_result(unsafe {
            pmp_shim_submit(
                self.hw.chan,
                u64::from(self.hw.ring_dma) + offset as u64,
                block_bytes as u32,
                self.hw.tx,
                if real { done_real } else { done_spare },
                ptr::from_ref(self).cast_mut().cast(),
            )
        });
        if ret.is_err() {
            self.r.inflight.fetch_sub(1, SeqCst);
        }
        ret
    }

    /// Stop by draining, not by terminating: stop submitting and let the queued
    /// transfers complete (at most `DEPTH` block times; the converters' clock
    /// never stops).  `dmaengine_terminate_sync()` disables the core mid-burst,
    /// which is rougher than needed; it remains the fallback for hardware that
    /// has really hung.  Caller holds `ctl`.
    fn stop(&self, ctl: &mut Ctl) {
        if !self.r.running.swap(false, SeqCst) {
            return;
        }
        // Pass through `top_lock` once: whoever holds it may have seen
        // `running` set; whoever takes it after us will not.
        while self.r.top_lock.swap(true, SeqCst) {
            core::hint::spin_loop();
        }
        self.r.top_lock.store(false, SeqCst);

        let mut waited = 0;
        while self.r.inflight.load(SeqCst) != 0 && waited < DRAIN_TIMEOUT_MS {
            fsleep(Delta::from_millis(1));
            waited += 1;
        }
        if self.r.inflight.load(SeqCst) != 0 {
            pr_warn!("pmpstream: {}: DMA did not drain; terminating\n", self.hw.name);
            // SAFETY: `chan` is ours; process context.
            unsafe { pmp_shim_terminate_sync(self.hw.chan) };
            fsleep(Delta::from_millis(DRAIN_SETTLE_MS));
        }

        if !self.hw.tx {
            self.rx_gate(false);
        }
        self.r.inflight.store(0, SeqCst);
        self.r.user.store(0, SeqCst);
        self.r.k_sub.store(0, SeqCst);
        self.r.k_done.store(0, SeqCst);
        self.r.spare_valid.store(false, SeqCst);
        ctl.rx_holding = false;
        self.wait.notify_all();
    }

    fn setup(&self, c: &Setup) -> Result {
        if self.r.running.load(SeqCst) || self.r.configured.load(SeqCst) {
            return Err(EBUSY);
        }
        // 8: the DMA moves whole 64-bit bus words.
        if c.block_bytes == 0
            || c.block_bytes > MAX_BLOCK
            || c.block_bytes & 7 != 0
            || c.nblocks < 2
            || c.nblocks > MAX_BLOCKS
            || c.fill > FILL_REPEAT
            || c.depth > MAX_DEPTH
            || (c.nblocks as usize + 1) * c.block_bytes as usize > POOL_BYTES
        {
            return Err(EINVAL);
        }
        // SAFETY: inside the pool, and nothing else is using it: not running,
        // and not mapped either (mmap needs `configured`).
        unsafe { ptr::write_bytes(self.hw.ring, 0, (c.nblocks as usize + 1) * c.block_bytes as usize) };
        self.r.block_bytes.store(c.block_bytes, SeqCst);
        self.r.nblocks.store(c.nblocks, SeqCst);
        self.r.fill.store(c.fill, SeqCst);
        self.r.depth.store(if c.depth == 0 { DEPTH } else { c.depth }, SeqCst);
        self.r.configured.store(true, SeqCst);
        Ok(())
    }

    /// Sleep until `ready()` or the stream stops.  The completion callback
    /// cannot take `ctl`, so a wake-up can slip between the check and the
    /// sleep; the one-jiffy timeout bounds what that costs.
    fn wait_for(&self, guard: &mut kernel::sync::lock::Guard<'_, Ctl, kernel::sync::lock::mutex::MutexBackend>, ready: impl Fn() -> bool) -> Result {
        while self.r.running.load(SeqCst) && !ready() {
            if let CondVarTimeoutResult::Signal { .. } = self.wait.wait_interruptible_timeout(guard, 1) {
                return Err(EINTR);
            }
        }
        Ok(())
    }

    fn xfer(&self) -> Xfer {
        Xfer {
            index: ring_index(self.r.user.load(SeqCst), self.r.nblocks.load(SeqCst)) as u32,
            bytes: self.r.block_bytes.load(SeqCst),
        }
    }
}

#[vtable]
impl MiscDevice for Stream {
    type Ptr = Arc<Stream>;

    fn open(_file: &File, misc: &MiscDeviceRegistration<Self>) -> Result<Arc<Stream>> {
        // SAFETY: every `MiscDeviceRegistration<Stream>` is the `misc` field of
        // a `StreamDev`, whose `stream` is initialised before it registers.
        let sd = unsafe { &*kernel::container_of!(ptr::from_ref(misc).cast_mut(), StreamDev, misc) };
        let s = &sd.stream;

        if s.r.busy.swap(true, SeqCst) {
            return Err(EBUSY);
        }
        // An open node must pin the module.  C drivers get that from
        // `file_operations.owner`, which the 6.18 misc-device abstraction
        // leaves unset, so without this `rmmod` with a node open would unload
        // the code under it.  (Doing it from inside the module leaves a
        // few-instruction window at either end that `owner` does not have.)
        // SAFETY: `THIS_MODULE` is this module.
        if !unsafe { bindings::try_module_get(crate::THIS_MODULE.as_ptr()) } {
            s.r.busy.store(false, SeqCst);
            return Err(ENODEV);
        }
        let restarts = s.r.stat_idle_restarts.swap(0, SeqCst);
        if restarts != 0 {
            pr_info!("pmpstream: {}: {} mid-stream restarts from idle last session\n", s.hw.name, restarts);
        }
        s.r.stat_blocks.store(0, SeqCst);
        s.r.stat_misses.store(0, SeqCst);
        Ok(s.clone())
    }

    fn release(s: Arc<Stream>, _file: &File) {
        // A dying daemon must not leave the DAC replaying its last block.
        let mut ctl = s.ctl.lock();
        s.stop(&mut ctl);
        s.r.configured.store(false, SeqCst);
        s.r.nblocks.store(0, SeqCst);
        drop(ctl);
        s.r.busy.store(false, SeqCst);
        drop(s);
        // SAFETY: pairs with the `try_module_get` in `open`.
        unsafe { bindings::module_put(crate::THIS_MODULE.as_ptr()) };
    }

    fn mmap(s: ArcBorrow<'_, Stream>, _file: &File, vma: &VmaNew) -> Result {
        let _ctl = s.ctl.lock();
        let len = vma.end() - vma.start();
        let want = s.r.block_bytes.load(SeqCst) as usize * s.r.nblocks.load(SeqCst) as usize;
        // SAFETY: `VmaNew` wraps a valid `vm_area_struct`.
        let pgoff = unsafe { (*vma.as_ptr()).vm_pgoff };
        if !s.r.configured.load(SeqCst) || pgoff != 0 || len != want.next_multiple_of(PAGE_SIZE) {
            return Err(EINVAL);
        }
        // The spare block past the end stays the kernel's.
        // SAFETY: the pool is a live coherent allocation of `dmadev`, at least
        // `len` long, and outlives the mapping's use of it as far as the DMA
        // API requires (it is never freed while a file is open).
        to_result(unsafe { bindings::dma_mmap_attrs(s.hw.dmadev, vma.as_ptr(), s.hw.ring.cast(), s.hw.ring_dma, len, 0) })
    }

    fn ioctl(s: ArcBorrow<'_, Stream>, _file: &File, cmd: u32, arg: usize) -> Result<isize> {
        let user = |size| UserSlice::new(UserPtr::from_addr(arg), size);

        // Never behind the mutex: the whole point of STATS is that a monitor
        // can ask "is it moving?" precisely when it is not.
        if cmd == IOC_STATS {
            let (u, d) = (s.r.user.load(SeqCst), s.r.k_done.load(SeqCst));
            let st = Stats {
                blocks: s.r.stat_blocks.load(SeqCst),
                misses: s.r.stat_misses.load(SeqCst),
                // Two separate loads: may be off by a block, never wildly.
                queued: if s.hw.tx { u.saturating_sub(d) } else { d.saturating_sub(u) } as u32,
                running: s.r.running.load(SeqCst).into(),
            };
            user(size_of::<Stats>()).writer().write(&st)?;
            return Ok(0);
        }

        let mut ctl = s.ctl.lock();
        match cmd {
            IOC_SETUP => s.setup(&user(size_of::<Setup>()).reader().read::<Setup>()?)?,

            IOC_START => {
                if !s.r.configured.load(SeqCst) {
                    return Err(ENXIO);
                }
                if !s.r.running.swap(true, SeqCst) {
                    s.top_up();
                }
            }

            IOC_STOP => s.stop(&mut ctl),

            IOC_COMMIT => {
                if !s.hw.tx || !s.r.configured.load(SeqCst) {
                    return Err(ENXIO);
                }
                let nblocks = u64::from(s.r.nblocks.load(SeqCst));
                // Before START nothing completes, so pre-buffering is bounded
                // by the ring: refuse rather than sleep forever.
                if !s.r.running.load(SeqCst) && (s.r.user.load(SeqCst) + 1).saturating_sub(s.r.k_done.load(SeqCst)) >= nblocks {
                    return Err(ENOSPC);
                }
                s.r.user.fetch_add(1, SeqCst);
                // The next block is ours once the hardware is done with it.
                // (Interrupted: the commit stands, only the wait is abandoned.)
                s.wait_for(&mut ctl, || s.r.user.load(SeqCst).saturating_sub(s.r.k_done.load(SeqCst)) < nblocks)?;
                user(size_of::<Xfer>()).writer().write(&s.xfer())?;
            }

            IOC_NEXT => {
                if s.hw.tx || !s.r.configured.load(SeqCst) {
                    return Err(ENXIO);
                }
                if ctl.rx_holding {
                    s.r.user.fetch_add(1, SeqCst);
                    ctl.rx_holding = false;
                }
                s.wait_for(&mut ctl, || s.r.k_done.load(SeqCst) != s.r.user.load(SeqCst))?;
                if !s.r.running.load(SeqCst) {
                    return Err(EPIPE);
                }
                ctl.rx_holding = true;
                user(size_of::<Xfer>()).writer().write(&s.xfer())?;
            }

            _ => return Err(ENOTTY),
        }
        Ok(0)
    }
}

/// One `/dev` node.  `misc` is declared first so that it is dropped -- the node
/// deregistered -- before this driver's reference to the stream goes.
#[pin_data]
struct StreamDev {
    #[pin]
    misc: MiscDeviceRegistration<Stream>,
    stream: Arc<Stream>,
}

impl StreamDev {
    fn new(stream: Arc<Stream>) -> impl PinInit<Self, Error> {
        let name = stream.hw.name;
        // `stream` first: `open()` may run the moment `misc` is registered.
        try_pin_init!(Self {
            stream,
            misc <- MiscDeviceRegistration::register(MiscDeviceOptions { name }),
        })
    }
}

#[pin_data]
struct PmpDriver {
    #[pin]
    tx: StreamDev,
    #[pin]
    rx: StreamDev,
}

kernel::of_device_table!(
    OF_TABLE,
    MODULE_OF_TABLE,
    <PmpDriver as platform::Driver>::IdInfo,
    [(of::DeviceId::new(c_str!("pmp,stream")), ())]
);

impl platform::Driver for PmpDriver {
    type IdInfo = ();
    const OF_ID_TABLE: Option<of::IdTable<Self::IdInfo>> = Some(&OF_TABLE);

    fn probe(pdev: &platform::Device<device::Core>, _info: Option<&Self::IdInfo>) -> Result<Pin<KBox<Self>>> {
        let dev = pdev.as_ref();
        let adc = dev.fwnode().and_then(|n| n.property_read::<u32>(c_str!("pmp,adc-core")).optional());
        let tx = Stream::new(dev, true, None)?;
        let rx = Stream::new(dev, false, adc)?;
        KBox::pin_init(
            try_pin_init!(Self {
                tx <- StreamDev::new(tx),
                rx <- StreamDev::new(rx),
            }),
            GFP_KERNEL,
        )
    }
}

kernel::module_platform_driver! {
    type: PmpDriver,
    name: "pmpstream",
    authors: ["pimpmypluto"],
    description: "pimpmypluto sample DMA rings",
    license: "GPL",
}
