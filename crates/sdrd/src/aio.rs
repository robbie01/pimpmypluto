//! Linux native AIO (io_setup/io_submit/io_getevents) for FunctionFS
//! endpoints, by raw syscall -- no libaio needed.
//!
//! Why AIO at all: a USB endpoint only moves data while a request is queued on
//! it.  With synchronous read()/write() there is exactly one, and between its
//! completion and the next submission the controller NAKs the host.  On this
//! UDC OUT requests must also stay within one transfer descriptor (16 KiB), so
//! the synchronous path costs a syscall round trip per 16 KiB and topped out
//! near 24 MB/s.  AIO keeps a whole queue of requests posted.
//!
//! Why not io_uring: FunctionFS's async path works with it, but its cancel hook
//! is wired to libaio only (kiocb_set_cancel_fn() ignores anything without
//! IOCB_AIO_RW), and this is the interface FunctionFS was written against.

use std::{fs::File, io, os::fd::AsRawFd};

#[repr(C)]
#[derive(Default)]
struct Iocb {
    data: u64,
    key: u32,
    rw_flags: u32,
    opcode: u16,
    reqprio: i16,
    fildes: u32,
    buf: u64,
    nbytes: u64,
    offset: i64,
    reserved2: u64,
    flags: u32,
    resfd: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct IoEvent {
    data: u64,
    obj: u64,
    res: i64,
    res2: i64,
}

/// io_getevents is a time32 syscall on 32-bit ARM; musl's timespec is time64.
#[repr(C)]
struct Timespec32 {
    sec: i32,
    nsec: i32,
}

const PREAD: u16 = 0;
const PWRITE: u16 = 1;

pub struct Slot {
    iocb: Box<Iocb>,
    pub buf: Vec<u8>,
    busy: bool,
}

pub struct Completion {
    pub slot: usize,
    /// Bytes transferred, or a negative errno.
    pub result: i64,
}

/// A fixed set of request slots on one endpoint file.
pub struct Queue {
    ctx: libc::c_ulong,
    fd: i32,
    _file: File,
    pub slots: Vec<Slot>,
    idle: Vec<usize>,
}

impl Queue {
    pub fn new(file: File, nslots: usize, bufsize: usize) -> io::Result<Self> {
        let mut ctx: libc::c_ulong = 0;
        // SAFETY: plain syscall with a valid out-pointer.
        if unsafe { libc::syscall(libc::SYS_io_setup, nslots as libc::c_long, &mut ctx) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let slots = (0..nslots).map(|_| Slot { iocb: Box::default(), buf: vec![0; bufsize], busy: false }).collect();
        Ok(Queue { ctx, fd: file.as_raw_fd(), _file: file, slots, idle: (0..nslots).rev().collect() })
    }

    pub fn idle_slot(&mut self) -> Option<usize> {
        self.idle.pop()
    }

    pub fn in_flight(&self) -> usize {
        self.slots.len() - self.idle.len()
    }

    fn submit(&mut self, slot: usize, opcode: u16, len: usize, tag: u64) -> io::Result<()> {
        let s = &mut self.slots[slot];
        debug_assert!(!s.busy && len <= s.buf.len());
        *s.iocb = Iocb {
            data: tag << 16 | slot as u64,
            opcode,
            fildes: self.fd as u32,
            buf: s.buf.as_mut_ptr() as u64,
            nbytes: len as u64,
            ..Default::default()
        };
        let mut p: *mut Iocb = &mut *s.iocb;
        // SAFETY: the iocb and its buffer are heap allocations owned by the slot,
        // which is marked busy (never touched or dropped) until its completion
        // has been reaped; Queue::drop destroys the context first.
        if unsafe { libc::syscall(libc::SYS_io_submit, self.ctx, 1 as libc::c_long, &mut p) } != 1 {
            self.idle.push(slot);
            return Err(io::Error::last_os_error());
        }
        s.busy = true;
        Ok(())
    }

    /// Post a read into the slot's whole buffer.  `tag` comes back in order-able form.
    pub fn read(&mut self, slot: usize, tag: u64) -> io::Result<()> {
        let len = self.slots[slot].buf.len();
        self.submit(slot, PREAD, len, tag)
    }

    /// Post a write of the first `len` bytes of the slot's buffer.
    pub fn write(&mut self, slot: usize, len: usize) -> io::Result<()> {
        self.submit(slot, PWRITE, len, 0)
    }

    /// Reap completions: waits up to `timeout_ms` for at least one.  Slots that
    /// complete are idle again only after the caller calls `release`.
    pub fn wait(&mut self, timeout_ms: u32, out: &mut Vec<(Completion, u64)>) -> io::Result<()> {
        let mut ev = [IoEvent::default(); 32];
        let mut ts = Timespec32 { sec: (timeout_ms / 1000) as i32, nsec: ((timeout_ms % 1000) * 1_000_000) as i32 };
        // SAFETY: valid buffers of the stated length.
        let n = unsafe { libc::syscall(libc::SYS_io_getevents, self.ctx, 1 as libc::c_long, ev.len() as libc::c_long, ev.as_mut_ptr(), &mut ts) };
        if n < 0 {
            let e = io::Error::last_os_error();
            return if e.kind() == io::ErrorKind::Interrupted { Ok(()) } else { Err(e) };
        }
        for e in &ev[..n as usize] {
            let slot = (e.data & 0xffff) as usize;
            self.slots[slot].busy = false;
            out.push((Completion { slot, result: e.res }, e.data >> 16));
        }
        Ok(())
    }

    pub fn release(&mut self, slot: usize) {
        self.idle.push(slot);
    }
}

impl Drop for Queue {
    fn drop(&mut self) {
        // io_destroy cancels what it can and waits for the rest, so no request
        // can touch a slot's buffer after this returns.
        // SAFETY: ctx came from io_setup.
        unsafe { libc::syscall(libc::SYS_io_destroy, self.ctx) };
    }
}
