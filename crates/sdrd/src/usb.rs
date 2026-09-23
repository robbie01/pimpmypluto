//! The pmpstream USB function, via FunctionFS: one vendor-class interface with
//! a bulk OUT (TX samples) and a bulk IN (RX samples) endpoint.  Control
//! requests arrive as events on ep0.

use pmp_proto as proto;
use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    os::fd::AsRawFd,
    path::Path,
};

const DESCRIPTORS_MAGIC_V2: u32 = 3;
const STRINGS_MAGIC: u32 = 2;
const HAS_FS_DESC: u32 = 1;
const HAS_HS_DESC: u32 = 2;
/// Deliver vendor requests addressed to the *device*, so hosts need neither
/// our interface number nor (on most systems) to claim anything for control.
const ALL_CTRL_RECIP: u32 = 64;

fn interface_and_endpoints(max_packet: u16) -> Vec<u8> {
    let mp = max_packet.to_le_bytes();
    let mut d = vec![
        9, 4, 0, 0, 2, proto::IF_CLASS, proto::IF_SUBCLASS, proto::IF_PROTOCOL, 1, // interface, iInterface = string 1
        7, 5, 0x01, 2, mp[0], mp[1], 0, // ep1 OUT bulk
        7, 5, 0x82, 2, mp[0], mp[1], 0, // ep2 IN bulk
    ];
    d.shrink_to_fit();
    d
}

fn descriptors() -> Vec<u8> {
    let (fs, hs) = (interface_and_endpoints(64), interface_and_endpoints(512));
    let mut b = Vec::new();
    b.extend(DESCRIPTORS_MAGIC_V2.to_le_bytes());
    b.extend(((12 + 8 + fs.len() + hs.len()) as u32).to_le_bytes());
    b.extend((HAS_FS_DESC | HAS_HS_DESC | ALL_CTRL_RECIP).to_le_bytes());
    b.extend(3u32.to_le_bytes()); // fs descriptor count
    b.extend(3u32.to_le_bytes()); // hs descriptor count
    b.extend(fs);
    b.extend(hs);
    b
}

fn strings() -> Vec<u8> {
    let s = b"pmpstream\0";
    let mut b = Vec::new();
    b.extend(STRINGS_MAGIC.to_le_bytes());
    b.extend(((16 + 2 + s.len()) as u32).to_le_bytes());
    b.extend(1u32.to_le_bytes()); // strings per language
    b.extend(1u32.to_le_bytes()); // languages
    b.extend(0x0409u16.to_le_bytes());
    b.extend(s);
    b
}

#[derive(Debug, Clone, Copy)]
pub struct Setup {
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
}

#[derive(Debug)]
pub enum Event {
    Bind,
    Unbind,
    Enable,
    Disable,
    Setup(Setup),
    Suspend,
    Resume,
}

pub struct Function {
    ep0: File,
}

impl Function {
    /// `dir` is a mounted functionfs instance.  Returns the function and its
    /// (OUT, IN) endpoint files.
    pub fn open(dir: &Path) -> io::Result<(Self, File, File)> {
        let mut ep0 = OpenOptions::new().read(true).write(true).open(dir.join("ep0"))?;
        ep0.write_all(&descriptors())?;
        ep0.write_all(&strings())?;
        let out = OpenOptions::new().read(true).open(dir.join("ep1"))?;
        let inp = OpenOptions::new().write(true).open(dir.join("ep2"))?;
        Ok((Function { ep0 }, out, inp))
    }

    /// Wait up to `timeout_ms` for an event.
    pub fn poll(&mut self, timeout_ms: i32) -> io::Result<Option<Event>> {
        let mut p = libc::pollfd { fd: self.ep0.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd.
        if unsafe { libc::poll(&mut p, 1, timeout_ms) } <= 0 || p.revents & libc::POLLIN == 0 {
            return Ok(None);
        }
        let mut e = [0u8; 12]; // struct usb_functionfs_event
        if self.ep0.read(&mut e)? < 12 {
            return Ok(None);
        }
        Ok(Some(match e[8] {
            0 => Event::Bind,
            1 => Event::Unbind,
            2 => Event::Enable,
            3 => Event::Disable,
            4 => Event::Setup(Setup {
                request_type: e[0],
                request: e[1],
                value: u16::from_le_bytes([e[2], e[3]]),
                index: u16::from_le_bytes([e[4], e[5]]),
                length: u16::from_le_bytes([e[6], e[7]]),
            }),
            5 => Event::Suspend,
            _ => Event::Resume,
        }))
    }

    /// Data stage of an OUT request (also acknowledges it).
    pub fn read_data(&mut self, len: usize) -> io::Result<Vec<u8>> {
        let mut b = vec![0u8; len];
        let n = self.ep0.read(&mut b)?;
        b.truncate(n);
        Ok(b)
    }

    /// Data stage of an IN request.
    pub fn reply(&mut self, s: &Setup, data: &[u8]) -> io::Result<()> {
        self.ep0.write_all(&data[..data.len().min(s.length as usize)])
    }

    /// Acknowledge an OUT request that has no data stage.
    pub fn ack(&mut self) -> io::Result<()> {
        self.ep0.read(&mut []).map(|_| ())
    }

    /// Refuse a request: FunctionFS stalls ep0 when the data stage is driven
    /// in the wrong direction.
    pub fn stall(&mut self, s: &Setup) {
        let _ = if s.request_type & 0x80 != 0 { self.ep0.read(&mut []).map(|_| ()) } else { self.ep0.write(&[]).map(|_| ()) };
    }
}
