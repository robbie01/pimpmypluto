//! Talk to a pimpmypluto radio over USB.
//!
//! ```no_run
//! use pmpstream::{Radio, StreamConfig, Format, TX};
//! use std::io::Write;
//!
//! let radio = Radio::open(None)?;
//! radio.configure(TX, &StreamConfig { lo_hz: 915_000_000, rate_hz: 4_000_000, gain_mdb: 10_000, ..Default::default() },
//!                 Format::Ci8, 0, 0)?;
//! let mut tx = radio.start_tx()?;
//! tx.write_all(&[0u8; 8192])?;          // interleaved int8 I,Q
//! # Ok::<(), pmpstream::Error>(())
//! ```
//!
//! Pure Rust (nusb): nothing to install on macOS or Linux.  The protocol is
//! small enough to use from anything that can do USB control and bulk
//! transfers; see docs/PROTOCOL.md.

use nusb::{
    transfer::{Bulk, ControlIn, ControlOut, ControlType, Direction, In, Out, Recipient},
    MaybeFuture,
};
pub use pmp_proto::{flags, Format, Info, Stats, StreamConfig, StreamOptions, RX, TX};
use pmp_proto::{self as proto, req};
use std::{fmt, time::Duration};

/// Retuning runs transceiver calibrations; be generous.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a freshly started RX stream may take to produce its marker.  The
/// device sends it before the first sample, so this only has to cover a
/// retune and whatever stale data the pipe still holds.
const RX_SYNC_TIMEOUT: Duration = Duration::from_secs(5);
const TRANSFER_SIZE: usize = 256 * 1024;

#[derive(Debug)]
pub enum Error {
    NotFound,
    Usb(String),
    /// The device accepted the transfer but refused the settings (out of
    /// range frequency, unsupported rate, ...).
    Rejected(&'static str),
    Protocol(&'static str),
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotFound => write!(f, "no pimpmypluto radio found on USB"),
            Error::Usb(e) => write!(f, "usb: {e}"),
            Error::Rejected(w) => write!(f, "device rejected {w}"),
            Error::Protocol(w) => write!(f, "protocol error: {w}"),
        }
    }
}
impl std::error::Error for Error {}
impl From<Error> for std::io::Error {
    fn from(e: Error) -> Self {
        std::io::Error::other(e.to_string())
    }
}
fn usb<E: fmt::Display>(e: E) -> Error {
    Error::Usb(e.to_string())
}

pub type TxStream = nusb::io::EndpointWrite<Bulk>;
pub type RxStream = nusb::io::EndpointRead<Bulk>;

pub struct Radio {
    device: nusb::Device,
    interface: nusb::Interface,
    ep_out: u8,
    ep_in: u8,
    serial: String,
}

/// Serial numbers of every attached radio.
pub fn list() -> Result<Vec<String>, Error> {
    Ok(nusb::list_devices()
        .wait()
        .map_err(usb)?
        .filter(|d| d.vendor_id() == proto::USB_VID && d.product_id() == proto::USB_PID)
        .map(|d| d.serial_number().unwrap_or("").to_string())
        .collect())
}

impl Radio {
    /// Open the radio with this serial number, or the first one found.
    pub fn open(serial: Option<&str>) -> Result<Self, Error> {
        let info = nusb::list_devices()
            .wait()
            .map_err(usb)?
            .find(|d| {
                d.vendor_id() == proto::USB_VID
                    && d.product_id() == proto::USB_PID
                    && serial.is_none_or(|s| d.serial_number() == Some(s))
            })
            .ok_or(Error::NotFound)?;
        let serial = info.serial_number().unwrap_or("").to_string();
        let device = info.open().wait().map_err(usb)?;

        // Find our interface by class triple rather than by number: it sits
        // after however many interfaces the network function takes.
        let config = device.active_configuration().map_err(usb)?;
        let (mut number, mut ep_out, mut ep_in) = (None, 0, 0);
        for alt in config.interface_alt_settings() {
            if (alt.class(), alt.subclass(), alt.protocol()) == (proto::IF_CLASS, proto::IF_SUBCLASS, proto::IF_PROTOCOL) {
                number = Some(alt.interface_number());
                for ep in alt.endpoints() {
                    match ep.direction() {
                        Direction::Out => ep_out = ep.address(),
                        Direction::In => ep_in = ep.address(),
                    }
                }
            }
        }
        let number = number.ok_or(Error::Protocol("device has no pmpstream interface"))?;
        let interface = device.claim_interface(number).wait().map_err(usb)?;
        let radio = Radio { device, interface, ep_out, ep_in, serial };
        let (info, _) = radio.info()?;
        if info.magic != proto::INFO_MAGIC || info.protocol_version != proto::PROTOCOL_VERSION {
            return Err(Error::Protocol("unexpected protocol version"));
        }
        Ok(radio)
    }

    pub fn serial(&self) -> &str {
        &self.serial
    }

    fn get(&self, request: u8, value: u16, length: u16) -> Result<Vec<u8>, Error> {
        self.device
            .control_in(
                ControlIn { control_type: ControlType::Vendor, recipient: Recipient::Device, request, value, index: 0, length },
                CONTROL_TIMEOUT,
            )
            .wait()
            .map_err(usb)
    }

    fn set(&self, request: u8, value: u16, data: &[u8]) -> Result<(), Error> {
        self.device
            .control_out(
                ControlOut { control_type: ControlType::Vendor, recipient: Recipient::Device, request, value, index: 0, data },
                CONTROL_TIMEOUT,
            )
            .wait()
            .map_err(usb)
    }

    /// Protocol info and the firmware version string.
    pub fn info(&self) -> Result<(Info, String), Error> {
        let b = self.get(req::GET_INFO, 0, 64)?;
        let info = Info::from_bytes(&b).ok_or(Error::Protocol("short GET_INFO"))?;
        Ok((info, String::from_utf8_lossy(&b[Info::SIZE..]).into_owned()))
    }

    /// Configure one direction ([`TX`] or [`RX`]); stops it if running.
    /// `flags` are [`flags`]; `prebuffer_blocks` 0 means the default.
    pub fn configure(&self, direction: u16, config: &StreamConfig, format: Format, flags: u8, prebuffer_blocks: u8) -> Result<(), Error> {
        let options = StreamOptions { format: format as u8, flags, prebuffer_blocks, reserved: 0 };
        let mut data = config.to_bytes().to_vec();
        data.extend(options.to_bytes());
        self.set(req::CONFIG, direction, &data)?;
        // A control OUT cannot fail after its data stage, so the device
        // reports a refused configuration by not having one: read it back.
        match self.get(req::CONFIG, direction, proto::CONFIG_SIZE as u16) {
            Ok(b) if b.len() >= proto::CONFIG_SIZE && b[..] == data[..] => Ok(()),
            _ => Err(Error::Rejected("stream configuration")),
        }
    }

    /// Start transmitting.  Returns the byte sink for samples in the
    /// configured format; writes block when the radio's buffers are full,
    /// which paces the producer to the DAC.  The DAC begins once the
    /// prebuffer has filled.
    pub fn start_tx(&self) -> Result<TxStream, Error> {
        self.start(TX)?;
        self.open_tx()
    }

    /// Start receiving.  Returns the byte source of samples in the configured
    /// format, positioned at the first sample of the new stream.
    pub fn start_rx(&self) -> Result<RxStream, Error> {
        self.start(RX)?;
        self.open_rx()
    }

    /// Start both directions with one request, so they begin together.
    pub fn start_both(&self) -> Result<(TxStream, RxStream), Error> {
        self.start(TX | RX)?;
        Ok((self.open_tx()?, self.open_rx()?))
    }

    /// The device STALLs START when a direction has no valid configuration or
    /// it cannot stream at all (its log says which).
    fn start(&self, directions: u16) -> Result<(), Error> {
        self.set(req::START, directions, &[]).map_err(|e| match e {
            Error::Usb(m) if m.contains("stall") => Error::Rejected("START (direction not configured, or the device cannot stream: see its log)"),
            e => e,
        })
    }

    pub fn stop(&self, directions: u16) -> Result<(), Error> {
        self.set(req::STOP, directions, &[])
    }

    // Each direction begins with pmp_proto::STREAM_MARKER; see there for why.

    fn open_tx(&self) -> Result<TxStream, Error> {
        use std::io::Write;
        let mut w = self.interface.endpoint::<Bulk, Out>(self.ep_out).map_err(usb)?.writer(TRANSFER_SIZE).with_num_transfers(4);
        w.write_all(&proto::STREAM_MARKER).map_err(usb)?;
        w.flush_end().map_err(usb)?; // a transfer of its own, ending in a short packet
        Ok(w)
    }

    fn open_rx(&self) -> Result<RxStream, Error> {
        use std::io::Read;
        let mut r = self.interface.endpoint::<Bulk, In>(self.ep_in).map_err(usb)?.reader(TRANSFER_SIZE).with_num_transfers(4);
        let mut scratch = vec![0u8; 64 * 1024];
        // Without a deadline a device that accepted START but cannot stream
        // leaves the caller waiting here for ever.
        const NO_MARKER: Error = Error::Protocol("the device accepted START but no stream arrived (see its log)");
        let deadline = std::time::Instant::now() + RX_SYNC_TIMEOUT;
        r.set_read_timeout(RX_SYNC_TIMEOUT);
        loop {
            if std::time::Instant::now() > deadline {
                return Err(NO_MARKER);
            }
            // one short-packet-terminated transfer; remember how it ended
            let mut tail = Vec::new();
            let mut message = r.until_short_packet();
            loop {
                let n = match message.read(&mut scratch) {
                    Err(e) if e.kind() == std::io::ErrorKind::TimedOut => return Err(NO_MARKER),
                    other => other.map_err(usb)?,
                };
                if n == 0 {
                    break;
                }
                tail.extend_from_slice(&scratch[..n]);
                let excess = tail.len().saturating_sub(proto::STREAM_MARKER.len());
                tail.drain(..excess);
            }
            message.consume_end().map_err(usb)?;
            if tail == proto::STREAM_MARKER {
                r.set_read_timeout(Duration::MAX); // from here on the caller paces reads
                return Ok(r);
            }
        }
    }

    pub fn stats(&self) -> Result<Stats, Error> {
        Stats::from_bytes(&self.get(req::GET_STATS, 0, Stats::SIZE as u16)?).ok_or(Error::Protocol("short GET_STATS"))
    }
}
