//! `sdrd serve`: the daemon.
//!
//!   control            ep0 events, radio configuration, health policy, watchdog
//!   usb-out -> tx      bulk OUT -> format conversion -> TX ring
//!   rx -> usb-in       RX ring -> format conversion -> bulk IN
//!
//! The sample threads own their rings; the control thread owns the radio.  They
//! meet in `Shared`: a config slot and a generation counter per direction.

use crate::{
    aio::Queue,
    radio::Radio,
    stream::{Fill, Monitor, Ring},
    usb::{Event, Function, Setup},
};
use pmp_convert::{pack, Unpacker};
use pmp_proto::{self as proto, flags, req, Format, StreamConfig, StreamOptions};
use std::{
    fs::File,
    io::Write,
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed},
        mpsc::{sync_channel, Receiver, SyncSender},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

const DEFAULT_BLOCK_SAMPLES: u32 = 1 << 18;
const RING_BLOCKS: usize = 8;
const DEFAULT_PREBUFFER: u8 = 3;
/// Continuous underrun for this long means the host is gone: stop radiating.
const HOST_GONE: Duration = Duration::from_secs(2);
/// A running ring whose block counters stand still for this long is a wedged
/// datapath; stop feeding the hardware watchdog and let the board reset.
const WEDGED: Duration = Duration::from_secs(5);

/// Highest complex sample rate each format can carry without dropping
/// samples, per direction, measured (docs/ARCHITECTURE.md).  A configuration
/// above its cap is rejected rather than accepted and then dropped from: a
/// dropped block is a silent failure, a refused CONFIG is a loud one.  The RX
/// caps are also the highest rates the board has survived for long: above
/// them the kernel has died under sustained bulk IN (open bug, same file).
fn max_rate_hz(which: u16, format: Format) -> u32 {
    match (which, format) {
        (proto::TX, Format::Ci16) => 8_000_000,
        (proto::TX, Format::Ci12) => 11_500_000,
        (proto::TX, Format::Ci8) => 16_000_000,
        (_, Format::Ci16) => 8_000_000,
        (_, Format::Ci12) => 11_000_000,
        (_, Format::Ci8) => 12_000_000, // 14 drops ~0.6 % in steady state; 12 was clean every time
    }
}

/// `sdrd serve --fake-rx`: the RX path is fed synthetic blocks instead of the
/// DMA ring, as fast as USB will take them.  A bug-hunting mode: everything
/// between the converter and the host is exercised, the radio is not.
static FAKE_RX: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy)]
struct Dir {
    cfg: StreamConfig,
    opt: StreamOptions,
}

#[derive(Default)]
struct DirState {
    config: Mutex<Option<Dir>>,
    /// Bumped by every START/STOP/CONFIG: tells the sample thread to rebuild.
    generation: AtomicU32,
    active: AtomicBool,
    /// The live ring's stats handle, so the control thread can watch a ring
    /// whose own thread is blocked on USB.  Cleared before the ring drops.
    monitor: Mutex<Option<Monitor>>,
    blocks: AtomicU64,
    misses: AtomicU64,
}

impl DirState {
    /// Refresh the counters from the live ring, if there is one.
    fn refresh(&self) {
        if let Some(Ok(s)) = self.monitor.lock().unwrap().as_ref().map(Monitor::stats) {
            self.blocks.store(s.blocks, Relaxed);
            self.misses.store(s.misses, Relaxed);
        }
    }
}

#[derive(Default)]
struct Shared {
    tx: DirState,
    rx: DirState,
}

fn dir_state(sh: &Shared, which: u16) -> &DirState {
    if which == proto::TX { &sh.tx } else { &sh.rx }
}

fn block_bytes(cfg: &StreamConfig) -> usize {
    let n = if cfg.block_samples == 0 { DEFAULT_BLOCK_SAMPLES } else { cfg.block_samples };
    n as usize * 4
}

fn format_of(d: &Dir) -> Format {
    Format::from_u8(d.opt.format).unwrap_or(Format::Ci16)
}

// ---------------------------------------------------------------------- tx --
//
//   usb_reader: keeps a read posted on the bulk OUT endpoint, always.
//   tx_thread:  converts what arrives into ring blocks.
//
// Two threads because a USB request must already be queued when the host's
// next transfer starts: a single read-convert-read loop leaves the endpoint
// NAKing for the length of every conversion, which caps throughput far below
// what either the bus or the CPU can do.
//
// Stream start: see pmp_proto::STREAM_MARKER.  (Cancelling the stale request
// instead is not an option: interrupting synchronous FunctionFS I/O races the
// UDC's completion against an on-stack completion object, and oopses.)

/// One ChipIdea transfer descriptor's worth, deliberately.  The UDC builds a
/// larger request from chained TDs, and a short packet ends only the TD it
/// lands in: the controller carries on into the next TD, the request completes
/// only when all of them have, and its buffer has a hole in it.  So within a
/// multi-TD OUT request short packets delimit nothing (the stream marker was
/// swallowed whole) and misplace data.  With single-TD requests a short packet
/// completes the request, as USB intends.
const USB_BUF: usize = 16 * 1024;
const USB_BUFS: usize = 64;
/// Requests kept queued on each endpoint (see aio.rs).
const USB_DEPTH: usize = 16;

fn usb_reader(ep: File, data: SyncSender<Vec<u8>>, recycle: Receiver<Vec<u8>>) {
    let mut q = match Queue::new(ep, USB_DEPTH, USB_BUF) {
        Ok(q) => q,
        Err(e) => return elog!("sdrd: usb-out: aio setup: {e}"),
    };
    // Requests on one endpoint complete in the order they were queued, but
    // FunctionFS delivers read completions through a workqueue, so the *events*
    // can overtake one another.  Number the requests and hand buffers on
    // strictly in sequence.
    let (mut next_tag, mut want) = (0u64, 0u64);
    let mut held = std::collections::BTreeMap::new();
    let mut done = Vec::new();
    loop {
        while let Some(slot) = q.idle_slot() {
            if q.read(slot, next_tag).is_err() {
                thread::sleep(Duration::from_millis(50)); // endpoint not enabled yet
                break;
            }
            next_tag += 1;
        }
        if q.wait(100, &mut done).is_err() {
            thread::sleep(Duration::from_millis(50));
        }
        for (c, tag) in done.drain(..) {
            held.insert(tag, c);
        }
        while let Some(c) = held.remove(&want) {
            want += 1;
            if c.result >= 0 {
                let Ok(mut v) = recycle.recv() else { return };
                v.clear();
                v.extend_from_slice(&q.slots[c.slot].buf[..c.result as usize]);
                if data.send(v).is_err() {
                    return;
                }
            } else {
                thread::sleep(Duration::from_millis(20)); // disabled / unplugged: do not spin
            }
            q.release(c.slot);
        }
    }
}

struct TxSession {
    generation: u32,
    /// Seen the marker transfer that precedes this stream's samples.
    synced: bool,
    discarded: u32,
    discarded_bytes: usize,
    ring: Ring,
    unpack: Unpacker,
    prebuffer: u32,
    filled: usize,
    committed: u32,
    started: bool,
}

impl TxSession {
    fn open(st: &DirState, generation: u32) -> Option<Self> {
        let dir = (*st.config.lock().unwrap())?;
        let fill = if dir.opt.flags & flags::TX_REPEAT_ON_UNDERRUN != 0 { Fill::Repeat } else { Fill::Zero };
        let ring = Ring::tx(block_bytes(&dir.cfg), RING_BLOCKS, fill).map_err(|e| elog!("sdrd: tx ring: {e}")).ok()?;
        *st.monitor.lock().unwrap() = ring.monitor().ok();
        let prebuffer = match dir.opt.prebuffer_blocks {
            0 => DEFAULT_PREBUFFER,
            n => n.min(RING_BLOCKS as u8 - 2),
        } as u32;
        Some(TxSession { generation, synced: false, discarded: 0, discarded_bytes: 0, ring, unpack: Unpacker::new(format_of(&dir)), prebuffer, filled: 0, committed: 0, started: false })
    }

    fn feed(&mut self, mut bytes: &[u8]) -> std::io::Result<()> {
        while !bytes.is_empty() {
            let block = self.ring.tx_block();
            let (used, made) = self.unpack.feed(bytes, &mut block[self.filled..]);
            bytes = &bytes[used..];
            self.filled += made;
            if self.filled == block.len() {
                self.ring.commit()?; // sleeps until the DAC has freed a block: this is the pacing
                self.filled = 0;
                self.committed += 1;
                if !self.started && self.committed >= self.prebuffer {
                    self.ring.start()?;
                    self.started = true;
                }
            }
        }
        Ok(())
    }
}

fn tx_thread(sh: Arc<Shared>, data: Receiver<Vec<u8>>, recycle: SyncSender<Vec<u8>>) {
    let st = &sh.tx;
    let mut session: Option<TxSession> = None;
    loop {
        let msg = data.recv_timeout(Duration::from_millis(20));
        let (generation, active) = (st.generation.load(Relaxed), st.active.load(Relaxed));
        if session.as_ref().is_some_and(|s| s.generation != generation || !active) {
            if let Some(s) = session.as_ref() {
                elog!("sdrd: tx gen {} ends: synced={} committed={} started={}", s.generation, s.synced, s.committed, s.started);
            }
            // Monitor first: the hardware stops when the last handle closes.
            *st.monitor.lock().unwrap() = None;
            session = None;
        }
        if active && session.is_none() {
            session = TxSession::open(st, generation);
            elog!("sdrd: tx gen {generation}: session {}", if session.is_some() { "opened" } else { "FAILED to open" });
            if session.is_none() {
                thread::sleep(Duration::from_millis(200));
            }
        }
        if let Ok(buf) = msg {
            // Samples sent while stopped are discarded, not queued.
            if session.is_none() && active {
                elog!("sdrd: tx: {} bytes arrived with no session open (marker: {})", buf.len(), buf.ends_with(&proto::STREAM_MARKER));
            }
            if let Some(s) = session.as_mut() {
                if !s.synced {
                    s.synced = buf.len() < USB_BUF && buf.ends_with(&proto::STREAM_MARKER);
                    s.discarded += 1;
                    s.discarded_bytes += buf.len();
                    if s.synced {
                        elog!("sdrd: tx gen {}: synced after discarding {} transfers / {} bytes (last {} bytes)",
                              s.generation, s.discarded, s.discarded_bytes, buf.len());
                    }
                } else if let Err(e) = s.feed(&buf) {
                    elog!("sdrd: tx: {e}");
                    *st.monitor.lock().unwrap() = None;
                    session = None;
                }
            }
            let _ = recycle.send(buf);
        }
    }
}

// ---------------------------------------------------------------------- rx --
//
//   rx_thread:  ring blocks -> wire format, tagged with the stream generation.
//   usb_writer: keeps a write posted on the bulk IN endpoint.
//
// The writer blocks for as long as the host is not reading.  A block left
// half-sent by a stream that ended is not cancelled: it is delivered ahead of
// the next stream's marker and discarded by the host.

/// Queue `bytes` as one write, waiting for a free slot if the host is slow.
/// Gives up (returns false) if the stream changes while waiting.
fn push(q: &mut Queue, bytes: &[u8], st: &DirState, generation: u32, done: &mut Vec<(crate::aio::Completion, u64)>) -> bool {
    loop {
        if let Some(slot) = q.idle_slot() {
            q.slots[slot].buf[..bytes.len()].copy_from_slice(bytes);
            if q.write(slot, bytes.len()).is_err() {
                thread::sleep(Duration::from_millis(50)); // endpoint disabled
            }
            return true;
        }
        let _ = q.wait(100, done);
        for (c, _) in done.drain(..) {
            q.release(c.slot);
        }
        if st.generation.load(Relaxed) != generation {
            return false;
        }
    }
}

fn usb_writer(sh: Arc<Shared>, ep: File, data: Receiver<(u32, Vec<u8>)>, recycle: SyncSender<Vec<u8>>) {
    let mut q = match Queue::new(ep, USB_DEPTH, USB_BUF) {
        Ok(q) => q,
        Err(e) => return elog!("sdrd: usb-in: aio setup: {e}"),
    };
    let mut done = Vec::new();
    let mut marked = None;
    while let Ok((generation, buf)) = data.recv() {
        if sh.rx.generation.load(Relaxed) == generation {
            if marked != Some(generation) {
                // Whatever an earlier stream left queued on this endpoint
                // reaches the host ahead of the marker, and is discarded there.
                push(&mut q, &proto::STREAM_MARKER, &sh.rx, generation, &mut done);
                marked = Some(generation);
            }
            for chunk in buf.chunks(USB_BUF) {
                if !push(&mut q, chunk, &sh.rx, generation, &mut done) {
                    break;
                }
            }
        }
        let _ = recycle.send(buf);
    }
}

/// The RX conversion step alone, for `sdrd rx-soak --work`.
pub fn pack_for_test(format: Format, block: &[i16], wire: &mut Vec<u8>) {
    pack(format, block, wire);
}

enum RxSource {
    Ring(Ring),
    /// One synthetic block, handed out over and over (see `FAKE_RX`).
    Fake(Vec<i16>),
}

fn rx_thread(sh: Arc<Shared>, out: SyncSender<(u32, Vec<u8>)>, pool: Receiver<Vec<u8>>) {
    let st = &sh.rx;
    let mut session: Option<(u32, RxSource, Format)> = None;
    let mut scratch: Vec<i16> = Vec::new();
    loop {
        let (generation, active) = (st.generation.load(Relaxed), st.active.load(Relaxed));
        if session.as_ref().is_some_and(|s| s.0 != generation || !active) {
            *st.monitor.lock().unwrap() = None;
            session = None;
        }
        if active && session.is_none() {
            session = (*st.config.lock().unwrap()).and_then(|dir| {
                if FAKE_RX.load(Relaxed) {
                    let n = block_bytes(&dir.cfg) / 2;
                    // An fs/8 tone, so a host looking for a signal finds one.
                    let block: Vec<i16> = (0..n).map(|i| [16000, 11314, 0, -11314, -16000, -11314, 0, 11314][(i / 2 + (i % 2) * 2) % 8]).collect();
                    return Some((generation, RxSource::Fake(block), format_of(&dir)));
                }
                let ring = Ring::rx(block_bytes(&dir.cfg), RING_BLOCKS).map_err(|e| elog!("sdrd: rx ring: {e}")).ok()?;
                ring.start().map_err(|e| elog!("sdrd: rx start: {e}")).ok()?;
                *st.monitor.lock().unwrap() = ring.monitor().ok();
                Some((generation, RxSource::Ring(ring), format_of(&dir)))
            });
        }
        let Some((_, src, format)) = session.as_mut() else {
            thread::sleep(Duration::from_millis(20));
            continue;
        };
        // No free buffer means the host is not reading: leave the ring to
        // count overruns, and keep checking whether we have been stopped.
        let Ok(mut wire) = pool.recv_timeout(Duration::from_millis(20)) else {
            // The fake source has no kernel ring counting overruns for it, so
            // a host that stops reading would look like a wedged datapath and
            // cost the board a watchdog reset.  Count the miss ourselves.
            if matches!(src, RxSource::Fake(_)) {
                st.misses.fetch_add(1, Relaxed);
            }
            continue;
        };
        let got: Result<&[i16], ()> = match src {
            RxSource::Ring(r) => r.next().map_err(|_| ()),
            RxSource::Fake(v) => {
                // No ring to report progress, so report it ourselves or the
                // health check will call the datapath wedged.
                st.blocks.fetch_add(1, Relaxed);
                Ok(&v[..])
            }
        };
        match got {
            Ok(block) => {
                // The ring is DMA-coherent, i.e. uncached: the converter's
                // sample-at-a-time loads each became a DRAM access and RX
                // topped out near 6.5 MS/s.  One bulk copy (long bursts) into
                // ordinary memory first, then convert from the cache.
                scratch.resize(block.len(), 0);
                scratch.copy_from_slice(block);
                pack(*format, &scratch, &mut wire);
                let _ = out.send((generation, wire));
            }
            Err(()) => {
                let _ = out.send((generation.wrapping_sub(1), wire)); // recycle via the writer
                *st.monitor.lock().unwrap() = None;
                session = None;
            }
        }
    }
}

// ----------------------------------------------------------------- control --

struct Control {
    radio: Radio,
    shared: Arc<Shared>,
    version: String,
    rate_hz: u32,
    muted: bool,
    /// When real (host-supplied) TX blocks last advanced, and the counters then.
    last_real: (Instant, u64, u64),
    last_progress: [(Instant, u64); 2],
}

impl Control {
    fn apply(&mut self, which: u16, dir: &Dir) -> Result<(), Box<dyn std::error::Error>> {
        let c = &dir.cfg;
        if Format::from_u8(dir.opt.format).is_none() || c.block_samples % 2 != 0 || block_bytes(c) > 4 << 20 {
            return Err("bad format or block size".into());
        }
        let cap = max_rate_hz(which, format_of(dir));
        if c.rate_hz > cap {
            return Err(format!("{} S/s exceeds this format's {} cap of {} S/s", c.rate_hz,
                               if which == proto::TX { "TX" } else { "RX" }, cap).into());
        }
        if c.rate_hz != self.rate_hz {
            self.radio.set_sample_rate(c.rate_hz)?;
            self.rate_hz = c.rate_hz;
        }
        let bw = if c.bandwidth_hz == 0 { c.rate_hz } else { c.bandwidth_hz };
        if which == proto::TX {
            self.radio.set_tx_lo(c.lo_hz)?;
            self.radio.set_tx_bandwidth(bw)?;
            self.radio.set_tx_attenuation(c.gain_mdb)?;
        } else {
            self.radio.set_rx_lo(c.lo_hz)?;
            self.radio.set_rx_bandwidth(bw)?;
            if dir.opt.flags & flags::RX_AGC != 0 {
                self.radio.set_rx_agc()?;
            } else {
                self.radio.set_rx_manual_gain((c.gain_mdb / 1000) as i32)?;
            }
        }
        Ok(())
    }

    fn set_active(&mut self, mask: u16, on: bool) -> Result<(), crate::radio::Error> {
        if mask & proto::TX != 0 {
            if on {
                self.radio.dac_from_dma()?;
            }
            self.radio.tx_lo_power(on)?;
            self.muted = false;
            self.last_real.0 = Instant::now();
            self.shared.tx.active.store(on, Relaxed);
            self.shared.tx.generation.fetch_add(1, Relaxed);
        }
        if mask & proto::RX != 0 {
            self.shared.rx.active.store(on, Relaxed);
            self.shared.rx.generation.fetch_add(1, Relaxed);
        }
        Ok(())
    }

    fn setup(&mut self, f: &mut Function, s: &Setup) -> std::io::Result<()> {
        match (s.request, s.request_type & 0x80 != 0) {
            (req::GET_INFO, true) => {
                let info = proto::Info { magic: proto::INFO_MAGIC, protocol_version: proto::PROTOCOL_VERSION, formats: 0b111 };
                let mut b = info.to_bytes().to_vec();
                b.extend(self.version.as_bytes());
                f.reply(s, &b)
            }
            (req::CONFIG, false) if s.value == proto::TX || s.value == proto::RX => {
                let data = f.read_data(s.length as usize)?;
                let parsed = StreamConfig::from_bytes(&data)
                    .zip(data.get(StreamConfig::SIZE..).and_then(StreamOptions::from_bytes));
                let Some((cfg, opt)) = parsed else {
                    elog!("sdrd: short CONFIG");
                    return Ok(());
                };
                let dir = Dir { cfg, opt };
                // The data stage is already acknowledged; a rejected config
                // is reported by START failing (no config stored).
                let _ = self.set_active(s.value, false);
                match self.apply(s.value, &dir) {
                    Ok(()) => *dir_state(&self.shared, s.value).config.lock().unwrap() = Some(dir),
                    Err(e) => {
                        elog!("sdrd: config rejected: {e}");
                        *dir_state(&self.shared, s.value).config.lock().unwrap() = None;
                    }
                }
                Ok(())
            }
            (req::CONFIG, true) if s.value == proto::TX || s.value == proto::RX => {
                match *dir_state(&self.shared, s.value).config.lock().unwrap() {
                    Some(d) => {
                        let mut b = d.cfg.to_bytes().to_vec();
                        b.extend(d.opt.to_bytes());
                        f.reply(s, &b)
                    }
                    None => Ok(f.stall(s)),
                }
            }
            (req::START, false) | (req::STOP, false) => {
                let on = s.request == req::START;
                let configured = [proto::TX, proto::RX]
                    .iter()
                    .all(|w| s.value & w == 0 || dir_state(&self.shared, *w).config.lock().unwrap().is_some());
                let available = (s.value & proto::TX == 0 || crate::stream::available(true))
                    && (s.value & proto::RX == 0 || crate::stream::available(false));
                if on && !available {
                    elog!("sdrd: START refused: no /dev/pmp-* (is the pmpstream module loaded?)");
                }
                if on && !(configured && available) || self.set_active(s.value, on).is_err() {
                    return Ok(f.stall(s));
                }
                f.ack()
            }
            (req::GET_STATS, true) => {
                let (tx, rx) = (&self.shared.tx, &self.shared.rx);
                let stats = proto::Stats {
                    tx_blocks: tx.blocks.load(Relaxed),
                    tx_underruns: tx.misses.load(Relaxed),
                    rx_blocks: rx.blocks.load(Relaxed),
                    rx_overruns: rx.misses.load(Relaxed),
                    state: tx.active.load(Relaxed) as u32 | (rx.active.load(Relaxed) as u32) << 1 | (self.muted as u32) << 2,
                    temperature_mc: self.radio.temperature_c().map(|t| (t * 1000.0) as i32).unwrap_or(i32::MIN),
                };
                f.reply(s, &stats.to_bytes())
            }
            _ => Ok(f.stall(s)),
        }
    }

    /// What used to need a supervisor script on the host.  Returns whether
    /// the datapath looks alive (i.e. whether to keep the watchdog fed).
    fn health(&mut self) -> bool {
        let now = Instant::now();
        self.shared.tx.refresh();
        self.shared.rx.refresh();
        let tx = &self.shared.tx;
        let (blocks, misses) = (tx.blocks.load(Relaxed), tx.misses.load(Relaxed));

        // Host gone: nothing but fill has gone out for HOST_GONE.  Stop
        // radiating it; resume by itself if the host picks up again.
        if blocks != self.last_real.1 {
            self.last_real = (now, blocks, misses);
            if self.muted && tx.active.load(Relaxed) {
                elog!("sdrd: host is sending again; unmuting");
                let _ = self.radio.tx_lo_power(true);
                self.muted = false;
            }
        } else if tx.active.load(Relaxed) && !self.muted && misses > self.last_real.2 && now - self.last_real.0 > HOST_GONE {
            elog!("sdrd: host stopped sending; muting transmitter");
            let _ = self.radio.tx_lo_power(false);
            self.muted = true;
        }

        // Wedged: a started ring moves a block (real or fill) several times a
        // second whatever the host does.  If the counters stand still, the
        // hardware has stopped and only a reset will bring it back.
        let mut alive = true;
        for (i, d) in [&self.shared.tx, &self.shared.rx].into_iter().enumerate() {
            let total = d.blocks.load(Relaxed) + d.misses.load(Relaxed);
            let (since, last) = self.last_progress[i];
            if total != last || !d.active.load(Relaxed) || total == 0 {
                self.last_progress[i] = (now, total);
            } else if now - since > WEDGED {
                alive = false;
            }
        }
        alive
    }
}

pub fn serve(ffs: &Path, fake_rx: bool) -> Result<(), Box<dyn std::error::Error>> {
    if fake_rx {
        FAKE_RX.store(true, Relaxed);
        elog!("sdrd: FAKE RX: the receive stream is synthetic (bug hunting), not the radio");
    }
    let radio = Radio::new()?;
    elog!("sdrd: radio up");
    let (mut function, ep_out, ep_in) = Function::open(ffs)?;
    elog!("sdrd: usb function registered");
    let shared = Arc::new(Shared::default());
    let version = std::fs::read_to_string("/etc/pmp-version").unwrap_or_default().trim().to_string();

    // TX: usb_reader -> tx_thread, buffers recycled back the other way.
    let (tx_data, tx_data_rx) = sync_channel(USB_BUFS);
    let (tx_free, tx_free_rx) = sync_channel(USB_BUFS);
    for _ in 0..USB_BUFS {
        tx_free.send(Vec::with_capacity(USB_BUF))?;
    }
    thread::Builder::new().name("usb-out".into()).spawn(move || usb_reader(ep_out, tx_data, tx_free_rx))?;
    let sh = shared.clone();
    thread::Builder::new().name("tx".into()).spawn(move || tx_thread(sh, tx_data_rx, tx_free))?;

    // RX: rx_thread -> usb_writer.  Few, block-sized buffers: anything queued
    // here is latency, and the ring already absorbs jitter.
    let (rx_data, rx_data_rx) = sync_channel(2);
    let (rx_free, rx_free_rx) = sync_channel(3);
    for _ in 0..3 {
        rx_free.send(Vec::new())?;
    }
    let sh = shared.clone();
    thread::Builder::new().name("usb-in".into()).spawn(move || usb_writer(sh, ep_in, rx_data_rx, rx_free))?;
    let sh = shared.clone();
    thread::Builder::new().name("rx".into()).spawn(move || rx_thread(sh, rx_data, rx_free_rx))?;

    let now = Instant::now();
    let mut c = Control {
        radio,
        shared,
        version,
        rate_hz: 0,
        muted: false,
        last_real: (now, 0, 0),
        last_progress: [(now, 0); 2],
    };
    c.radio.tx_lo_power(false)?;

    // From here on this process is what keeps the board from resetting.
    let mut watchdog = File::options().write(true).open("/dev/watchdog").ok();
    // Descriptors are written: tell /init it can bind the gadget now.
    std::fs::write("/run/sdrd.ready", b"")?;
    elog!("sdrd: serving");

    let mut heartbeat = Instant::now();
    loop {
        if heartbeat.elapsed() > Duration::from_secs(10) {
            heartbeat = Instant::now();
            let (t, r) = (&c.shared.tx, &c.shared.rx);
            elog!("sdrd: alive  tx {}/{}{}  rx {}/{}{}", t.blocks.load(Relaxed), t.misses.load(Relaxed),
                  if t.active.load(Relaxed) { " on" } else { "" }, r.blocks.load(Relaxed), r.misses.load(Relaxed),
                  if r.active.load(Relaxed) { " on" } else { "" });
        }
        match function.poll(500)? {
            Some(Event::Setup(s)) => {
                if let Err(e) = c.setup(&mut function, &s) {
                    elog!("sdrd: control request {:#04x}: {e}", s.request);
                }
            }
            Some(Event::Disable) | Some(Event::Unbind) => {
                // cable pulled or host reset the device: go quiet
                let _ = c.set_active(proto::TX | proto::RX, false);
            }
            _ => {}
        }
        if c.health() {
            if let Some(w) = watchdog.as_mut() {
                let _ = w.write_all(b"\0");
            }
        } else {
            elog!("sdrd: datapath wedged; withholding watchdog");
        }
    }
}
