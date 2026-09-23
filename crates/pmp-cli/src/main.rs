//! pmp -- stream samples to and from a pimpmypluto radio.
//!
//!     ntsctx --out /dev/stdout clip.mp4 | pmp tx --freq 471.25e6 --rate 6797203 --format ci8
//!     pmp rx --freq 100e6 --rate 4e6 --format ci16 --seconds 2 > capture.ci16

use clap::{Args, Parser, Subcommand, ValueEnum};
use pmpstream::{flags, Format, Radio, StreamConfig, RX, TX};
use std::{
    io::{self, Read, Write},
    time::{Duration, Instant},
};

#[derive(Parser)]
#[command(name = "pmp", about = "Stream samples to and from a pimpmypluto radio")]
struct Cli {
    /// Serial number of the radio to use (default: the first one found)
    #[arg(long, global = true)]
    serial: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List attached radios
    List,
    /// Firmware and protocol information
    Info,
    /// Stream counters and transceiver temperature
    Stats,
    /// Transmit samples from a file or stdin
    Tx(TxArgs),
    /// Receive samples to a file or stdout
    Rx(RxArgs),
    /// Transmit a tone and look for it with the radio's own receiver
    Loopback(LoopArgs),
}

#[derive(Clone, Copy, ValueEnum)]
enum Fmt {
    Ci8,
    Ci12,
    Ci16,
}
impl From<Fmt> for Format {
    fn from(f: Fmt) -> Self {
        match f {
            Fmt::Ci8 => Format::Ci8,
            Fmt::Ci12 => Format::Ci12,
            Fmt::Ci16 => Format::Ci16,
        }
    }
}

/// Accept 471.25e6, 6_797_203, 915M, 4k ...
fn hz(s: &str) -> Result<f64, String> {
    let s = s.replace('_', "");
    let (num, mult) = match s.chars().last() {
        Some('k' | 'K') => (&s[..s.len() - 1], 1e3),
        Some('M') => (&s[..s.len() - 1], 1e6),
        Some('G') => (&s[..s.len() - 1], 1e9),
        _ => (&s[..], 1.0),
    };
    num.parse::<f64>().map(|v| v * mult).map_err(|e| e.to_string())
}

#[derive(Args, Clone)]
struct Common {
    /// Centre frequency, Hz
    #[arg(long, value_parser = hz)]
    freq: f64,
    /// Complex sample rate, Hz
    #[arg(long, value_parser = hz)]
    rate: f64,
    /// Analog bandwidth, Hz (default: the sample rate)
    #[arg(long, value_parser = hz)]
    bw: Option<f64>,
    /// Sample format on the wire
    #[arg(long, value_enum, default_value = "ci16")]
    format: Fmt,
    /// Samples per hardware block (latency, and the unit of underrun fill)
    #[arg(long, default_value_t = 0)]
    block_samples: u32,
}

#[derive(Args)]
struct LoopArgs {
    #[command(flatten)]
    c: Common,
    /// Receive in a different format than --format (which TX uses)
    #[arg(long, value_enum)]
    rx_format: Option<Fmt>,
}

#[derive(Args)]
struct TxArgs {
    #[command(flatten)]
    c: Common,
    /// Attenuation below full power, dB
    #[arg(long, default_value_t = 10.0)]
    atten: f64,
    /// On underrun repeat the last block instead of sending silence
    #[arg(long)]
    repeat_on_underrun: bool,
    /// Blocks to buffer in the radio before the DAC starts
    #[arg(long, default_value_t = 0)]
    prebuffer: u8,
    /// Ignore the input and transmit a tone this far from the centre, Hz
    #[arg(long, value_parser = hz, allow_hyphen_values = true)]
    tone: Option<f64>,
    /// Play the input again when it ends
    #[arg(long = "loop")]
    looping: bool,
    /// Input file ('-' or omitted: stdin)
    input: Option<String>,
}

#[derive(Args)]
struct RxArgs {
    #[command(flatten)]
    c: Common,
    /// Manual gain, dB
    #[arg(long, default_value_t = 30.0)]
    gain: f64,
    /// Automatic gain control instead of --gain
    #[arg(long)]
    agc: bool,
    /// Stop after this many seconds (default: until interrupted)
    #[arg(long)]
    seconds: Option<f64>,
    /// Output file ('-' or omitted: stdout)
    output: Option<String>,
}

type Res<T> = Result<T, Box<dyn std::error::Error>>;

fn config(c: &Common, gain_mdb: u32) -> StreamConfig {
    StreamConfig {
        lo_hz: c.freq as u64,
        rate_hz: c.rate as u32,
        bandwidth_hz: c.bw.unwrap_or(0.0) as u32,
        gain_mdb,
        block_samples: c.block_samples,
    }
}

/// Wire bytes for one second's worth of a complex tone that loops seamlessly.
fn tone_bytes(format: Format, rate: f64, offset: f64, amplitude: f64) -> Vec<u8> {
    let n = rate as usize;
    let cycles = (offset * n as f64 / rate).round();
    let native: Vec<i16> = (0..n)
        .flat_map(|k| {
            let ph = std::f64::consts::TAU * cycles * k as f64 / n as f64;
            [(ph.cos() * 2047.0 * amplitude) as i16, (ph.sin() * 2047.0 * amplitude) as i16]
        })
        .collect();
    let mut wire = Vec::new();
    pmp_convert::pack(format, &native, &mut wire);
    wire
}

fn report(radio: &Radio, last: &mut Instant, bytes: u64, t0: Instant, bps: usize) {
    if last.elapsed() < Duration::from_secs(2) {
        return;
    }
    *last = Instant::now();
    if let Ok(s) = radio.stats() {
        eprintln!(
            "{:.3} MS/s   tx {} blocks, {} underruns   rx {} blocks, {} overruns   {:.1} C{}",
            bytes as f64 / bps as f64 / t0.elapsed().as_secs_f64() / 1e6,
            s.tx_blocks, s.tx_underruns, s.rx_blocks, s.rx_overruns,
            s.temperature_mc as f64 / 1000.0,
            if s.state & 4 != 0 { "   [muted: host too slow]" } else { "" }
        );
    }
}

fn tx(radio: &Radio, a: TxArgs) -> Res<()> {
    let format: Format = a.c.format.into();
    let fl = if a.repeat_on_underrun { flags::TX_REPEAT_ON_UNDERRUN } else { 0 };
    radio.configure(TX, &config(&a.c, (a.atten * 1000.0) as u32), format, fl, a.prebuffer)?;
    let mut w = radio.start_tx()?;
    let (t0, mut last, mut total) = (Instant::now(), Instant::now(), 0u64);
    let bps = format.bytes_per_sample();

    if let Some(offset) = a.tone {
        let wire = tone_bytes(format, a.c.rate, offset, 0.5);
        loop {
            w.write_all(&wire)?;
            total += wire.len() as u64;
            report(radio, &mut last, total, t0, bps);
        }
    }
    let path = a.input.as_deref().filter(|p| *p != "-");
    let mut buf = vec![0u8; 1 << 18];
    loop {
        let mut input: Box<dyn Read> = match path {
            Some(p) => Box::new(std::fs::File::open(p)?),
            None => Box::new(io::stdin().lock()),
        };
        loop {
            let n = input.read(&mut buf)?;
            if n == 0 {
                break;
            }
            w.write_all(&buf[..n])?;
            total += n as u64;
            report(radio, &mut last, total, t0, bps);
        }
        if !(a.looping && path.is_some()) {
            break;
        }
    }
    w.flush()?;
    // let the radio's buffers drain before the transmitter is switched off
    std::thread::sleep(Duration::from_millis(1500));
    radio.stop(TX)?;
    Ok(())
}

fn rx(radio: &Radio, a: RxArgs) -> Res<()> {
    let format: Format = a.c.format.into();
    radio.configure(RX, &config(&a.c, (a.gain * 1000.0) as u32), format, if a.agc { flags::RX_AGC } else { 0 }, 0)?;
    let mut r = radio.start_rx()?;
    let mut out: Box<dyn Write> = match a.output.as_deref().filter(|p| *p != "-") {
        Some(p) => Box::new(std::fs::File::create(p)?),
        None => Box::new(io::stdout().lock()),
    };
    let bps = format.bytes_per_sample();
    let want = a.seconds.map(|s| (s * a.c.rate) as u64 * bps as u64);
    let (t0, mut last, mut total) = (Instant::now(), Instant::now(), 0u64);
    let mut buf = vec![0u8; 1 << 18];
    while want.is_none_or(|w| total < w) {
        let room = want.map_or(buf.len(), |w| ((w - total) as usize).min(buf.len()));
        let n = r.read(&mut buf[..room])?;
        out.write_all(&buf[..n])?;
        total += n as u64;
        report(radio, &mut last, total, t0, bps);
    }
    radio.stop(RX)?;
    Ok(())
}

/// TX a tone at +rate/8 through USB and the DAC, receive it back through the
/// ADC and USB, and check that is what arrived.
fn loopback(radio: &Radio, a: LoopArgs) -> Res<()> {
    let c = a.c;
    let tx_format: Format = c.format.into();
    let format: Format = a.rx_format.unwrap_or(c.format).into();
    let bps = format.bytes_per_sample();
    radio.configure(TX, &config(&c, 10_000), tx_format, 0, 0)?;
    radio.configure(RX, &config(&c, 20_000), format, 0, 0)?;
    let (mut w, mut r) = radio.start_both()?;
    let wire = tone_bytes(tx_format, c.rate, c.rate / 8.0, 0.5);

    let wire = &wire;
    let got = std::thread::scope(|s| -> Res<Vec<u8>> {
        let feeder = s.spawn(move || -> io::Result<()> {
            for _ in 0..5 {
                w.write_all(&wire)?;
            }
            w.flush()
        });
        let mut cap = vec![0u8; (c.rate as usize * 3) * bps];
        r.read_exact(&mut cap)?;
        feeder.join().unwrap()?;
        Ok(cap)
    })?;
    let stats = radio.stats()?;
    radio.stop(TX | RX)?;

    // analyse the last 2^16 samples: by then the TX prebuffer has drained through
    let mut native = vec![0i16; 2 << 16];
    let tail = &got[got.len() - (1 << 16) * bps..];
    let mut u = pmp_convert::Unpacker::new(format);
    u.feed(tail, &mut native);
    let (mut ci, mut cq, mut total) = (0f64, 0f64, 0f64);
    for (n, iq) in native.chunks_exact(2).enumerate() {
        let (i, q) = (iq[0] as f64, iq[1] as f64);
        let ph = -std::f64::consts::TAU * (n % 8) as f64 / 8.0;
        ci += i * ph.cos() - q * ph.sin();
        cq += i * ph.sin() + q * ph.cos();
        total += i * i + q * q;
    }
    let ratio = (ci * ci + cq * cq) / (total * (1 << 16) as f64).max(1.0);
    println!("tx {} blocks, {} underruns; rx {} blocks, {} overruns", stats.tx_blocks, stats.tx_underruns, stats.rx_blocks, stats.rx_overruns);
    println!("tone carries {:.1}% of received power", ratio * 100.0);
    if ratio < 0.5 {
        return Err("tone not found".into());
    }
    println!("PASS: host -> USB ({tx_format:?}) -> DAC -> air -> ADC -> USB ({format:?}) -> host");
    Ok(())
}

fn main() {
    let cli = Cli::parse();
    let run = || -> Res<()> {
        if let Cmd::List = cli.cmd {
            for s in pmpstream::list()? {
                println!("{s}");
            }
            return Ok(());
        }
        let radio = Radio::open(cli.serial.as_deref())?;
        match cli.cmd {
            Cmd::List => unreachable!(),
            Cmd::Info => {
                let (info, version) = radio.info()?;
                println!("serial   {}\nfirmware {version}\nprotocol {}\nformats  {:#05b}", radio.serial(), info.protocol_version, info.formats);
                Ok(())
            }
            Cmd::Stats => {
                println!("{:#?}", radio.stats()?);
                Ok(())
            }
            Cmd::Tx(a) => tx(&radio, a),
            Cmd::Rx(a) => rx(&radio, a),
            Cmd::Loopback(c) => loopback(&radio, c),
        }
    };
    if let Err(e) = run() {
        eprintln!("pmp: {e}");
        std::process::exit(1);
    }
}
