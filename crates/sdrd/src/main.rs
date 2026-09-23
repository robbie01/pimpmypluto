/// One line, one write(): stderr is /dev/kmsg under init, where every write
/// is a separate (and rate-limited) record.
macro_rules! elog {
    ($($arg:tt)*) => {{
        use std::io::Write;
        let _ = std::io::stderr().write_all(format!("{}\n", format_args!($($arg)*)).as_bytes());
    }};
}

mod aio;
mod noos;
mod platform;
mod radio;
mod serve;
mod stream;
mod turnaround;
mod usb;

use radio::Radio;

fn arg<T: std::str::FromStr>(args: &[String], name: &str, default: T) -> T {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.replace('_', "").parse().ok())
        .unwrap_or(default)
}

/// Bring the radio up, put a DDS tone on the air and look for it with the
/// board's own receiver: RSSI with the TX LO on versus off.
fn radio_test(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let freq: u64 = arg(args, "--freq", 915_000_000);
    let rate: u32 = arg(args, "--rate", 4_000_000);
    let hold: u64 = arg(args, "--hold", 0);

    let mut r = Radio::new()?;
    println!("ad936x up, {:.1} C", r.temperature_c()?);
    println!("sample rate {} Hz", r.set_sample_rate(rate)?);
    r.set_bandwidth(rate)?;
    println!("tx lo {} Hz, rx lo {} Hz", r.set_tx_lo(freq)?, r.set_rx_lo(freq)?);
    r.set_tx_attenuation(10_000)?;
    r.set_rx_manual_gain(30)?;
    r.dds_tone(rate / 8, 0.5)?;

    let mut measure = |on: bool| -> Result<f32, radio::Error> {
        r.tx_lo_power(on)?;
        std::thread::sleep(std::time::Duration::from_millis(300));
        r.rx_rssi_db()
    };
    let off = measure(false)?;
    let on = measure(true)?;
    println!("rx rssi: tx off {off:.1} dB, tx on {on:.1} dB, delta {:+.1} dB", on - off);
    if hold > 0 {
        println!("holding tone for {hold} s");
        std::thread::sleep(std::time::Duration::from_secs(hold));
    }
    r.tx_lo_power(false)?;
    if on - off < 6.0 {
        return Err("no tone seen by the receiver".into());
    }
    println!("PASS: own receiver sees the tone");
    Ok(())
}

/// End-to-end check of the sample datapath with no host involved: a tone
/// written into the TX ring must come back through the RX ring, and neither
/// ring may miss a block while we keep up.
fn loop_test(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    use stream::{Fill, Ring};
    let freq: u64 = arg(args, "--freq", 915_000_000);
    let rate: u32 = arg(args, "--rate", 4_000_000);
    let seconds: u32 = arg(args, "--seconds", 3);
    const BLOCK: usize = 1 << 20;

    let mut r = Radio::new()?;
    r.set_sample_rate(rate)?;
    r.set_bandwidth(rate)?;
    r.set_tx_lo(freq)?;
    r.set_rx_lo(freq)?;
    r.set_tx_attenuation(10_000)?;
    r.set_rx_manual_gain(20)?;
    r.dac_from_dma()?;
    r.tx_lo_power(true)?;

    // fs/8 tone: period of 8 samples, so every block is phase-continuous.
    let lut: [(i16, i16); 8] = std::array::from_fn(|n| {
        let ph = std::f64::consts::TAU * n as f64 / 8.0;
        ((ph.cos() * 16000.0) as i16, (ph.sin() * 16000.0) as i16)
    });
    let mut tx = Ring::tx(BLOCK, 8, Fill::Zero)?;
    let mut rx = Ring::rx(BLOCK, 8)?;
    let fill = move |b: &mut [i16]| {
        for (n, iq) in b.chunks_exact_mut(2).enumerate() {
            (iq[0], iq[1]) = lut[n % 8];
        }
    };
    for _ in 0..3 {
        fill(tx.tx_block());
        tx.commit()?;
    }
    tx.start()?;
    rx.start()?;

    // Feed for as long as the receiver is looking (plus slack), and time it:
    // how fast the ring drains *is* the DAC's real sample rate.
    let feeder = std::thread::spawn(move || -> std::io::Result<(stream::Stats, f64)> {
        let t0 = std::time::Instant::now();
        let mut fed = 0u64;
        while t0.elapsed().as_secs_f64() < seconds as f64 + 1.0 {
            fill(tx.tx_block());
            tx.commit()?;
            fed += tx.block_samples() as u64;
        }
        Ok((tx.stats()?, fed as f64 / t0.elapsed().as_secs_f64()))
    });

    // Power in the +fs/8 bin versus everything, over the last block captured.
    // Integer rotation by multiples of 90 degrees per two samples would do for
    // fs/4; for fs/8 use a table -- per-sample trig is too slow on this core.
    let rot: [(f32, f32); 8] = std::array::from_fn(|n| {
        let ph = -std::f32::consts::TAU * n as f32 / 8.0;
        (ph.cos(), ph.sin())
    });
    let (mut ratio, mut peak) = (0.0f32, 0i16);
    let mut bins = [0f32; 8];
    let blocks = seconds as usize * rate as usize / rx.block_samples();
    for _ in 0..blocks.max(2) {
        let b = rx.next()?;
        let (mut ci, mut cq, mut total) = (0f32, 0f32, 0f32);
        const N: usize = 1 << 14;
        peak = 0;
        for (n, iq) in b.chunks_exact(2).enumerate().take(N) {
            let (i, q) = (iq[0] as f32, iq[1] as f32);
            let (c, s) = rot[n % 8];
            ci += i * c - q * s;
            cq += i * s + q * c;
            total += i * i + q * q;
            peak = peak.max(iq[0].abs()).max(iq[1].abs());
        }
        ratio = (ci * ci + cq * cq) / (total * N as f32).max(1.0);
        for (k, bin) in bins.iter_mut().enumerate() {
            let (mut ci, mut cq) = (0f32, 0f32);
            for (n, iq) in b.chunks_exact(2).enumerate().take(N) {
                let (c, s) = rot[(n * k) % 8];
                ci += iq[0] as f32 * c - iq[1] as f32 * s;
                cq += iq[0] as f32 * s + iq[1] as f32 * c;
            }
            *bin = (ci * ci + cq * cq) / (total * N as f32).max(1.0);
        }
    }
    println!("chip rssi while the tx ring is on air: {:.1} dB", r.rx_rssi_db()?);
    println!("first rx samples (I,Q): {:?}", rx.next()?.chunks_exact(2).take(10).map(|c| (c[0], c[1])).collect::<Vec<_>>());
    println!("power by bin k*fs/8: {}", bins.iter().map(|b| format!("{:.0}%", b * 100.0)).collect::<Vec<_>>().join(" "));
    println!("rx peak sample {peak} (12-bit ADC, full scale 2047)");
    let rxs = rx.stats()?;
    let (txs, tx_rate) = feeder.join().unwrap()?;
    println!("dac drained the tx ring at {:.3} MS/s (configured {:.3})", tx_rate / 1e6, rate as f64 / 1e6);
    r.tx_lo_power(false)?;
    println!("tx: {} blocks, {} underruns   rx: {} blocks, {} overruns", txs.blocks, txs.misses, rxs.blocks, rxs.misses);
    println!("tone carries {:.1}% of received power", ratio * 100.0);
    if ratio < 0.5 {
        return Err("tone not found in received samples".into());
    }
    if txs.misses + rxs.misses > 0 {
        return Err("rings missed blocks with an idle system".into());
    }
    println!("PASS: tx ring -> DAC -> air -> ADC -> rx ring");
    Ok(())
}

/// Diagnostic: busy-poll a DMA controller's live address registers and shout
/// (into the kernel log, which survives a crash) if it is ever pointed outside
/// the memory it was given.  `dmac-watch DMAC_BASE POOL_BASE POOL_BYTES`
fn dmac_watch(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::fd::AsRawFd;
    let num = |i: usize| -> Result<u64, Box<dyn std::error::Error>> {
        let a = args.get(i).ok_or("usage: sdrd dmac-watch DMAC_BASE POOL_BASE POOL_BYTES")?;
        Ok(u64::from_str_radix(a.trim_start_matches("0x"), 16)?)
    };
    let (dmac, pool, len) = (num(0)?, num(1)?, num(2)?);
    let mem = std::fs::OpenOptions::new().read(true).write(true).open("/dev/mem")?;
    // SAFETY: mapping one page of device registers, read-only use.
    let base = unsafe { libc::mmap(std::ptr::null_mut(), 4096, libc::PROT_READ, libc::MAP_SHARED, mem.as_raw_fd(), dmac as libc::off_t) };
    if base == libc::MAP_FAILED {
        return Err("mmap /dev/mem".into());
    }
    let reg = |off: usize| -> u64 { unsafe { (base as *const u32).byte_add(off).read_volatile() as u64 } };
    let mut kmsg = std::fs::OpenOptions::new().write(true).open("/dev/kmsg")?;
    let mut say = |m: String| {
        use std::io::Write;
        let _ = kmsg.write_all(m.as_bytes());
    };
    say(format!("dmac-watch: {dmac:#x}, pool {pool:#x}+{len:#x}\n"));
    let (mut lo, mut hi, mut polls, mut bad) = (u64::MAX, 0u64, 0u64, 0u32);
    let mut tick = std::time::Instant::now();
    loop {
        for _ in 0..4096 {
            // 0x410 DEST_ADDRESS (queued next), 0x438 CURRENT_DEST_ADDR (in flight)
            for off in [0x438usize, 0x410] {
                let a = reg(off);
                // 0: reset value.  0x1ffffff8: what the driver's probe leaves in
                // DEST_ADDRESS (all-ones, read back through the address mask)
                // until the first transfer programs it.
                if a == 0 || (off == 0x410 && a == 0x1fff_fff8) {
                    continue;
                }
                lo = lo.min(a);
                hi = hi.max(a);
                if (a < pool || a >= pool + len) && bad < 20 {
                    bad += 1;
                    say(format!("dmac-watch: OUT OF POOL reg {off:#x} = {a:#010x}  ctrl={:#x} id={:#x} xlen={:#x} status={:#x}\n",
                                reg(0x400), reg(0x42c), reg(0x418), reg(0x430)));
                }
            }
            polls += 1;
        }
        if tick.elapsed().as_secs() >= 10 {
            tick = std::time::Instant::now();
            say(format!("dmac-watch: {polls} polls, addresses seen {lo:#010x}..{hi:#010x}, {bad} out of pool\n"));
        }
    }
}

/// Diagnostic: RX streaming sessions with no USB sample traffic at all.
fn rx_soak(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let sessions: u32 = arg(args, "--sessions", 60);
    let seconds: u64 = arg(args, "--seconds", 3);
    let rate: u32 = arg(args, "--rate", 4_000_000);
    let freq: u64 = arg(args, "--freq", 915_000_000);
    // 0: configure the radio once.  1: re-apply LO/bandwidth/gain between
    // sessions, as every host session does.  2: also while the DMA is running.
    let retune: u32 = arg(args, "--retune", 0);
    // --work: also copy and pack every block to ci8, as `serve` does, so the
    // CPU is as busy as in a real session -- everything but USB.
    let work: u32 = arg(args, "--work", 0);
    let mut scratch: Vec<i16> = Vec::new();
    let mut wire: Vec<u8> = Vec::new();
    let mut r = Radio::new()?;
    r.set_sample_rate(rate)?;
    r.set_bandwidth(rate)?;
    r.set_rx_lo(freq)?;
    r.set_rx_manual_gain(20)?;
    elog!("rx-soak: retune mode {retune}");
    let mut reconfigure = |r: &mut Radio, n: u32| -> Result<(), radio::Error> {
        r.set_rx_lo(freq + (n as u64 % 2) * 1_000_000)?;
        r.set_rx_bandwidth(rate)?;
        r.set_rx_manual_gain(20 + (n % 2) as i32)
    };
    for n in 1..=sessions {
        if retune >= 1 {
            reconfigure(&mut r, n)?;
        }
        let mut rx = stream::Ring::rx(1 << 20, 8)?;
        rx.start()?;
        let t = std::time::Instant::now();
        let mut sum = 0i64;
        let mut k = 0;
        while t.elapsed().as_secs() < seconds {
            let block = rx.next()?;
            if work != 0 {
                scratch.resize(block.len(), 0);
                scratch.copy_from_slice(block);
                wire.resize(block.len(), 0);
                serve::pack_for_test(pmp_proto::Format::Ci8, &scratch, &mut wire);
                sum += wire.iter().step_by(4096).map(|v| *v as i64).sum::<i64>();
            } else {
                sum += block.iter().step_by(4096).map(|v| *v as i64).sum::<i64>();
            }
            k += 1;
            if retune >= 2 && k % 8 == 0 {
                reconfigure(&mut r, n + k)?;
            }
        }
        let st = rx.stats()?;
        elog!("rx-soak: session {n}/{sessions}: {} blocks, {} overruns (checksum {sum})", st.blocks, st.misses);
    }
    elog!("rx-soak: all {sessions} sessions completed");
    Ok(())
}

/// Correctness and speed of the format converters on this CPU.
fn convert_test() -> Result<(), Box<dyn std::error::Error>> {
    use pmp_convert::{pack, Unpacker};
    use pmp_proto::Format;
    let n = 1 << 20;
    let native: Vec<i16> = (0..n)
        .flat_map(|k| {
            let ph = std::f64::consts::TAU * (k % 8) as f64 / 8.0;
            [(ph.cos() * 1023.0) as i16, (ph.sin() * 1023.0) as i16]
        })
        .collect();
    let mut failed = false;
    for f in [Format::Ci16, Format::Ci8, Format::Ci12] {
        let mut wire = Vec::new();
        let t = std::time::Instant::now();
        pack(f, &native, &mut wire);
        let pack_rate = n as f64 / t.elapsed().as_secs_f64() / 1e6;

        let mut u = Unpacker::new(f);
        let mut block = vec![0i16; 1 << 19];
        let (mut filled, mut got) = (0, Vec::with_capacity(native.len()));
        let t = std::time::Instant::now();
        for usb in wire.chunks(256 * 1024) {
            let mut bytes = usb;
            while !bytes.is_empty() {
                let (used, made) = u.feed(bytes, &mut block[filled..]);
                bytes = &bytes[used..];
                filled += made;
                if filled == block.len() {
                    got.extend_from_slice(&block);
                    filled = 0;
                }
            }
        }
        let unpack_rate = n as f64 / t.elapsed().as_secs_f64() / 1e6;
        got.extend_from_slice(&block[..filled]);
        let bad = got.iter().zip(&native).position(|(g, n)| match f {
            Format::Ci8 => *g != ((n >> 4) as i8 as i16) << 8,
            _ => *g != n << 4,
        });
        println!("{f:?}: unpack {unpack_rate:.1} MS/s, pack {pack_rate:.1} MS/s, {}",
                 match bad { None => "correct".to_string(), Some(i) => format!("WRONG from value {i} (got {}, want {})", got[i], native[i] << 4) });
        failed |= bad.is_some() || got.len() != native.len();
    }
    if failed { Err("conversion self-test failed".into()) } else { Ok(()) }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let r = match args.first().map(String::as_str) {
        Some("radio-test") => radio_test(&args[1..]),
        Some("loop-test") => loop_test(&args[1..]),
        Some("convert-test") => convert_test(),
        Some("dmac-watch") => dmac_watch(&args[1..]),
        Some("rx-soak") => rx_soak(&args[1..]),
        Some("turnaround-test") => turnaround::turnaround_test(&args[1..]),
        Some("serve") => serve::serve(
            std::path::Path::new(args[1..].iter().find(|a| !a.starts_with("--")).map(String::as_str).unwrap_or("/dev/pmp-ffs")),
            args.iter().any(|a| a == "--fake-rx"),
        ),
        _ => {
            eprintln!("usage: sdrd serve [FUNCTIONFS_DIR] [--fake-rx]\n       sdrd radio-test [--freq HZ] [--rate HZ] [--hold SECONDS]\n       sdrd loop-test  [--freq HZ] [--rate HZ] [--seconds N]\n       sdrd convert-test\n       sdrd turnaround-test [--freq HZ] [--rate HZ] [--trials N] [--blocks N,N,...]");
            std::process::exit(2);
        }
    };
    if let Err(e) = r {
        eprintln!("sdrd: {e}");
        std::process::exit(1);
    }
}
