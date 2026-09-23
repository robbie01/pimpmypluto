//! `sdrd turnaround-test`: how fast can software answer a received burst?
//!
//! The measurement behind the HaLow question in docs/GATEWARE-EVAL.md.  The
//! board talks to itself through its own TX->RX leakage.  Per trial:
//!
//!   1. transmit a stimulus burst S (one ring block of tone);
//!   2. watch the RX stream for S; when the block holding its *end* arrives,
//!      commit a response burst R at once -- what an ACK after a frame does;
//!   3. keep watching; R shows up in the same RX stream.
//!
//! Turnaround = (onset of R) - (end of S), counted in RX samples, so it is
//! exact to a sample and needs no clock but the converter's.  It contains
//! everything a real responder would pay -- RX block fill, DMA completion,
//! wake-up, this code, the TX DMA queue, the DAC path -- plus the ADC path a
//! second time (R has to come back to be seen), a few tens of samples.

use crate::radio::Radio;
use crate::stream::{Fill, Ring};
use std::time::{Duration, Instant};

/// pmpstream keeps this many transfers queued in the TX DMA by default; a
/// committed block goes out after whatever is already queued.
const DMA_DEPTH: usize = 4;

fn realtime_on_cpu1() {
    // SAFETY: plain syscalls on our own thread with valid arguments.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(1, &mut set);
        if libc::sched_setaffinity(0, std::mem::size_of_val(&set), &set) != 0 {
            eprintln!("turnaround: could not pin to CPU1");
        }
        let mut p: libc::sched_param = std::mem::zeroed();
        p.sched_priority = 80;
        // By raw syscall: musl's sched_setscheduler() is a stub that fails.
        if libc::syscall(libc::SYS_sched_setscheduler, 0, libc::SCHED_FIFO, &p) != 0 {
            eprintln!("turnaround: could not get SCHED_FIFO");
        }
        libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE);
    }
}

fn mag(iq: &[i16]) -> u32 {
    iq[0].unsigned_abs() as u32 + iq[1].unsigned_abs() as u32
}

struct Outcome {
    turnaround: Vec<u32>, // samples, one per valid trial
    software_us: Vec<u32>, // NEXT returning -> COMMIT returned
    discarded: u32,
    timeouts: u32,
    rx_misses: u64,
    tx_blocks_sent: u64,
}

fn run(block: usize, trials: u32, rate: u32, depth: usize) -> Result<Outcome, Box<dyn std::error::Error>> {
    let mut tx = Ring::tx_with_depth(block * 4, 16, Fill::Zero, depth)?;
    let mut rx = Ring::rx(block * 4, 64)?;
    let tone = |b: &mut [i16]| {
        const LUT: [(i16, i16); 8] =
            [(16000, 0), (11314, 11314), (0, 16000), (-11314, 11314), (-16000, 0), (-11314, -11314), (0, -16000), (11314, -11314)];
        for (n, iq) in b.chunks_exact_mut(2).enumerate() {
            (iq[0], iq[1]) = LUT[n % 8];
        }
    };
    // Nothing is committed except bursts: between them the kernel transmits
    // its zero fill block, which is exactly the idle channel we want.
    tx.start()?;
    rx.start()?;

    // Noise floor, then one burst to find the signal level.
    let settle = (rate as usize / 20).div_ceil(block); // 50 ms
    let mut noise = 0u32;
    for k in 0..settle * 2 {
        let b = rx.next()?;
        if k >= settle {
            noise = noise.max(b.chunks_exact(2).map(mag).max().unwrap_or(0));
        }
    }
    tone(tx.tx_block());
    tx.commit()?;
    let mut peak = 0u32;
    for _ in 0..settle * 2 {
        peak = peak.max(rx.next()?.chunks_exact(2).map(mag).max().unwrap_or(0));
    }
    let thr = (noise * 3).max(peak / 4);
    if peak < noise * 6 {
        return Err(format!("burst not visible in own receiver (noise {noise}, burst {peak})").into());
    }

    #[derive(PartialEq, Clone, Copy)]
    enum St { Idle, SRise, SFall, RRise, RFall }
    let mut out = Outcome { turnaround: vec![], software_us: vec![], discarded: 0, timeouts: 0, rx_misses: 0, tx_blocks_sent: 0 };
    let (mut st, mut n, mut quiet, mut s_end, mut idle_until) = (St::Idle, 0u64, 0u32, 0u64, 0u64);
    let mut deadline = 0u64;
    let mut misses_at_start = rx.stats()?.misses;
    let gap = rate as u64 / 50; // 20 ms between trials
    let mut done = 0;
    while done < trials {
        let b = rx.next()?;
        let t_block = Instant::now();
        let mut respond = false;
        let mut r_onset = None;
        for iq in b.chunks_exact(2) {
            let hot = mag(iq) > thr;
            quiet = if hot { 0 } else { quiet + 1 };
            match st {
                St::SRise if hot => st = St::SFall,
                // 8 quiet samples = one tone period: the burst is over.
                St::SFall if quiet == 8 => {
                    s_end = n - 7;
                    respond = true;
                    st = St::RRise;
                }
                St::RRise if hot => {
                    r_onset = Some((n - s_end) as u32);
                    st = St::RFall;
                }
                St::RFall if quiet == 8 => {
                    st = St::Idle;
                    idle_until = n + gap;
                }
                _ => {}
            }
            n += 1;
        }
        if let Some(t) = r_onset {
            if rx.stats()?.misses == misses_at_start {
                out.turnaround.push(t);
            } else {
                out.discarded += 1; // lost RX blocks: the sample count is off
            }
            done += 1;
        }
        if respond {
            tone(tx.tx_block());
            tx.commit()?;
            out.tx_blocks_sent += 1;
            out.software_us.push(t_block.elapsed().as_micros() as u32);
        }
        if st == St::Idle && n >= idle_until {
            misses_at_start = rx.stats()?.misses;
            tone(tx.tx_block());
            tx.commit()?;
            out.tx_blocks_sent += 1;
            st = St::SRise;
            deadline = n + rate as u64; // 1 s
        }
        if st != St::Idle && n > deadline {
            out.timeouts += 1;
            done += 1;
            st = St::Idle;
            idle_until = n + gap;
        }
    }
    out.rx_misses = rx.stats()?.misses;
    Ok(out)
}

pub fn turnaround_test(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let freq: u64 = crate::arg(args, "--freq", 915_000_000);
    let rate: u32 = crate::arg(args, "--rate", 4_000_000);
    let trials: u32 = crate::arg(args, "--trials", 200);
    let blocks: String = crate::arg(args, "--blocks", "4096,1024,512,256,128,64".to_string());
    let depth: usize = crate::arg(args, "--depth", DMA_DEPTH);

    let mut r = Radio::new()?;
    let rate = r.set_sample_rate(rate)?;
    r.set_bandwidth(rate)?;
    r.set_tx_lo(freq)?;
    r.set_rx_lo(freq)?;
    r.set_tx_attenuation(10_000)?;
    r.set_rx_manual_gain(20)?;
    r.dac_from_dma()?;
    r.tx_lo_power(true)?;
    realtime_on_cpu1();

    let us = |samples: f64| samples * 1e6 / rate as f64;
    println!("turnaround: end of received burst -> onset of our reply, as seen by our own receiver");
    println!("rate {rate} S/s, {trials} trials per block size, SCHED_FIFO on CPU1, TX DMA depth {depth}");
    println!("{:>7} {:>9} | {:>8} {:>8} {:>8} {:>8} | {:>9} | {:>7} {:>5} {:>5} {:>8}",
             "block", "block us", "min us", "median", "p99", "max", "sw med us", "valid", "disc", "t/o", "rx miss");
    for b in blocks.split(',').filter_map(|s| s.trim().parse::<usize>().ok()) {
        let t0 = Instant::now();
        match run(b, trials, rate, depth) {
            Ok(mut o) => {
                o.turnaround.sort_unstable();
                o.software_us.sort_unstable();
                let t = &o.turnaround;
                let pick = |v: &[u32], q: f64| v.get(((v.len() as f64 - 1.0) * q) as usize).copied().unwrap_or(0) as f64;
                println!("{:>7} {:>9.1} | {:>8.0} {:>8.0} {:>8.0} {:>8.0} | {:>9.0} | {:>7} {:>5} {:>5} {:>8}",
                         b, us(b as f64), us(pick(t, 0.0)), us(pick(t, 0.5)), us(pick(t, 0.99)), us(pick(t, 1.0)),
                         pick(&o.software_us, 0.5), t.len(), o.discarded, o.timeouts, o.rx_misses);
            }
            Err(e) => println!("{b:>7} failed: {e}"),
        }
        if t0.elapsed() > Duration::from_secs(120) {
            println!("(stopping: a block size took over two minutes)");
            break;
        }
    }
    r.tx_lo_power(false)?;
    Ok(())
}
