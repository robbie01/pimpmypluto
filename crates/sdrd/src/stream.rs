//! Rust face of the pmpstream kernel rings (os/kernel/pmpstream/pmpstream.h).

use std::{fs::File, io, os::fd::AsRawFd, ptr::NonNull};

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Setup { block_bytes: u32, nblocks: u32, fill: u32, depth: u32 }
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Xfer { index: u32, bytes: u32 }
#[repr(C)]
#[derive(Default, Clone, Copy, Debug)]
pub struct Stats { pub blocks: u64, pub misses: u64, pub queued: u32, pub running: u32 }

const fn ioc(dir: u32, nr: u32, size: usize) -> libc::c_ulong {
    ((dir << 30) | ((size as u32) << 16) | ((b'p' as u32) << 8) | nr) as libc::c_ulong
}
const SETUP: libc::c_ulong = ioc(1, 1, size_of::<Setup>());
const START: libc::c_ulong = ioc(0, 2, 0);
const COMMIT: libc::c_ulong = ioc(3, 4, size_of::<Xfer>());
const NEXT: libc::c_ulong = ioc(3, 5, size_of::<Xfer>());
const STATS: libc::c_ulong = ioc(2, 6, size_of::<Stats>());

fn map_len(block_bytes: usize, nblocks: usize) -> usize {
    (block_bytes * nblocks).next_multiple_of(4096)
}

#[derive(Clone, Copy, PartialEq)]
pub enum Fill { Zero = 0, Repeat = 1 }

/// One direction's ring: `nblocks` blocks of `block_bytes`, mapped contiguously.
pub struct Ring {
    file: File,
    base: NonNull<u8>,
    block_bytes: usize,
    nblocks: usize,
    current: usize,
}
// The mapping belongs to this handle alone.
unsafe impl Send for Ring {}

const TX_NODE: &str = "/dev/pmp-tx";
const RX_NODE: &str = "/dev/pmp-rx";

/// Whether the kernel side of a direction exists at all (the pmpstream module
/// is loaded and probed).  Rings are opened by the sample threads after START
/// has been acknowledged; this is what START itself can still refuse on.
pub fn available(tx: bool) -> bool {
    std::path::Path::new(if tx { TX_NODE } else { RX_NODE }).exists()
}

impl Ring {
    fn open(path: &str, block_bytes: usize, nblocks: usize, fill: Fill, depth: usize) -> io::Result<Self> {
        let file = File::options().read(true).write(true).open(path)?;
        let setup = Setup { block_bytes: block_bytes as u32, nblocks: nblocks as u32, fill: fill as u32, depth: depth as u32 };
        // SAFETY: ioctl/mmap on our own fd with correctly sized arguments.
        unsafe {
            if libc::ioctl(file.as_raw_fd(), SETUP as _, &setup) < 0 {
                return Err(io::Error::last_os_error());
            }
            let p = libc::mmap(std::ptr::null_mut(), map_len(block_bytes, nblocks), libc::PROT_READ | libc::PROT_WRITE,
                               libc::MAP_SHARED, file.as_raw_fd(), 0);
            if p == libc::MAP_FAILED {
                return Err(io::Error::last_os_error());
            }
            Ok(Ring { file, base: NonNull::new_unchecked(p.cast()), block_bytes, nblocks, current: 0 })
        }
    }

    pub fn tx(block_bytes: usize, nblocks: usize, fill: Fill) -> io::Result<Self> {
        Self::open(TX_NODE, block_bytes, nblocks, fill, 0)
    }

    /// TX with an explicit DMA queue depth (see pmpstream.h); for burst
    /// responders, where a committed block must go out soon.
    pub fn tx_with_depth(block_bytes: usize, nblocks: usize, fill: Fill, depth: usize) -> io::Result<Self> {
        Self::open(TX_NODE, block_bytes, nblocks, fill, depth)
    }

    pub fn rx(block_bytes: usize, nblocks: usize) -> io::Result<Self> {
        Self::open(RX_NODE, block_bytes, nblocks, Fill::Zero, 0)
    }

    fn simple(&self, cmd: libc::c_ulong) -> io::Result<()> {
        // SAFETY: argument-less ioctl.
        if unsafe { libc::ioctl(self.file.as_raw_fd(), cmd as _) } < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }
    pub fn start(&self) -> io::Result<()> { self.simple(START) }

    pub fn block_samples(&self) -> usize { self.block_bytes / 4 }

    fn block(&mut self, i: usize) -> &mut [i16] {
        // SAFETY: block i lies inside the mapping; i16 alignment holds because
        // the mapping is page aligned and block_bytes is a multiple of 8.
        unsafe { std::slice::from_raw_parts_mut(self.base.as_ptr().add(i * self.block_bytes).cast(), self.block_bytes / 2) }
    }

    /// TX: the block to fill next, as interleaved I,Q.
    pub fn tx_block(&mut self) -> &mut [i16] { self.block(self.current) }

    /// TX: hand the filled block to the hardware; blocks until another is free.
    pub fn commit(&mut self) -> io::Result<()> {
        let mut x = Xfer::default();
        // SAFETY: valid pointer to a correctly sized struct.
        if unsafe { libc::ioctl(self.file.as_raw_fd(), COMMIT as _, &mut x) } < 0 {
            return Err(io::Error::last_os_error());
        }
        self.current = x.index as usize;
        Ok(())
    }

    /// RX: wait for the next received block (releasing the previous one).
    pub fn next(&mut self) -> io::Result<&[i16]> {
        let mut x = Xfer::default();
        // SAFETY: as above.
        if unsafe { libc::ioctl(self.file.as_raw_fd(), NEXT as _, &mut x) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(self.block(x.index as usize))
    }

    pub fn stats(&self) -> io::Result<Stats> {
        stats_of(&self.file)
    }

    /// A second handle on the same open ring, for reading stats from another
    /// thread while this one is blocked.  It must be dropped before the ring:
    /// the kernel stops the hardware when the *last* handle closes.
    pub fn monitor(&self) -> io::Result<Monitor> {
        Ok(Monitor(self.file.try_clone()?))
    }
}

pub struct Monitor(File);
impl Monitor {
    pub fn stats(&self) -> io::Result<Stats> {
        stats_of(&self.0)
    }
}

fn stats_of(f: &File) -> io::Result<Stats> {
    let mut s = Stats::default();
    // SAFETY: valid pointer to a correctly sized struct.
    if unsafe { libc::ioctl(f.as_raw_fd(), STATS as _, &mut s) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(s)
}

impl Drop for Ring {
    fn drop(&mut self) {
        // SAFETY: unmapping exactly what `open` mapped; the fd closes after,
        // which stops the hardware and frees the ring.
        unsafe { libc::munmap(self.base.as_ptr().cast(), map_len(self.block_bytes, self.nblocks)) };
    }
}
