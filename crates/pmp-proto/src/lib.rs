//! The pmpstream USB protocol.  The normative description is docs/PROTOCOL.md;
//! this crate is the single definition both ends compile against.
//!
//! Everything is little-endian.  Control messages are fixed-size structs sent
//! as the data stage of vendor control transfers; samples are raw on the bulk
//! endpoints in the negotiated [`Format`].
#![no_std]

pub const USB_VID: u16 = 0x1209; // pid.codes
pub const USB_PID: u16 = 0x0001; // pid.codes test PID -- replace before distributing
pub const IF_CLASS: u8 = 0xFF;
pub const IF_SUBCLASS: u8 = 0x50; // 'P'
pub const IF_PROTOCOL: u8 = 0x01;
pub const PROTOCOL_VERSION: u16 = 1;

/// bRequest values.  bmRequestType is vendor | device (0x40 out, 0xC0 in),
/// wIndex 0.
pub mod req {
    /// IN, [`Info`](super::Info).
    pub const GET_INFO: u8 = 0x01;
    /// OUT/IN, [`StreamConfig`](super::StreamConfig).  wValue = [`TX`](super::TX) or [`RX`](super::RX).
    /// Setting a config stops that direction first.
    pub const CONFIG: u8 = 0x10;
    /// OUT, no data.  wValue = bitmask of directions.
    pub const START: u8 = 0x20;
    pub const STOP: u8 = 0x21;
    /// IN, [`Stats`](super::Stats).
    pub const GET_STATS: u8 = 0x30;
}

/// Start-of-stream marker, sent as a transfer of its own in each direction
/// after START: by the host on the OUT endpoint before TX samples, by the
/// device on the IN endpoint before RX samples.  The receiver discards
/// everything up to and including the first *short* transfer that ends with
/// these bytes.
///
/// Why: bulk pipes have no framing, and a transfer abandoned by an earlier
/// stream (a host that exited mid-write, a host that stopped reading) is still
/// sitting on the endpoint.  Its bytes would otherwise lead the new stream --
/// always a multiple of the packet size, so invisible to 2- and 4-byte samples
/// and a permanent misalignment for 3-byte ones.
pub const STREAM_MARKER: [u8; 8] = *b"PMPSTRM1";

pub const TX: u16 = 1;
pub const RX: u16 = 2;

/// Sample formats on the bulk endpoints.  All are complex, I then Q, and all
/// are *full scale at the type's range*: +127, +2047 and +32767 mean the same
/// amplitude.  The converters are 12-bit, so ci12 is lossless and ci16 carries
/// four bits of padding.
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    /// int16 I, int16 Q: 4 bytes per sample.
    Ci16 = 0,
    /// int8 I, int8 Q: 2 bytes per sample.
    Ci8 = 1,
    /// 12-bit I and Q packed in 3 bytes: `I[7:0]`, `Q[3:0]<<4 | I[11:8]`, `Q[11:4]`.
    Ci12 = 2,
}

impl Format {
    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Ci16),
            1 => Some(Self::Ci8),
            2 => Some(Self::Ci12),
            _ => None,
        }
    }
    pub const fn bytes_per_sample(self) -> usize {
        match self {
            Self::Ci16 => 4,
            Self::Ci8 => 2,
            Self::Ci12 => 3,
        }
    }
}

/// [`StreamConfig::flags`]
pub mod flags {
    /// TX: on underrun repeat the last block instead of sending silence.
    /// With `block_samples` set to one frame of a periodic signal this is a
    /// freeze-frame rather than a dropout.
    pub const TX_REPEAT_ON_UNDERRUN: u8 = 1 << 0;
    /// RX: automatic gain control (slow attack); `gain_mdb` is ignored.
    pub const RX_AGC: u8 = 1 << 1;
}

macro_rules! wire_struct {
    ($(#[$m:meta])* pub struct $name:ident { $($(#[$fm:meta])* pub $f:ident : $t:ty),* $(,)? }) => {
        $(#[$m])*
        #[repr(C)]
        #[derive(Clone, Copy, Debug, Default, PartialEq)]
        pub struct $name { $($(#[$fm])* pub $f: $t),* }
        impl $name {
            pub const SIZE: usize = core::mem::size_of::<Self>();
            pub fn to_bytes(&self) -> [u8; core::mem::size_of::<Self>()] {
                let mut out = [0u8; core::mem::size_of::<Self>()];
                let mut at = 0;
                $( let b = self.$f.to_le_bytes(); out[at..at + b.len()].copy_from_slice(&b); at += b.len(); )*
                let _ = at;
                out
            }
            pub fn from_bytes(buf: &[u8]) -> Option<Self> {
                if buf.len() < Self::SIZE { return None; }
                let mut at = 0;
                $( let n = core::mem::size_of::<$t>();
                   let mut b = [0u8; core::mem::size_of::<$t>()];
                   b.copy_from_slice(&buf[at..at + n]);
                   let $f = <$t>::from_le_bytes(b);
                   at += n; )*
                let _ = at;
                Some(Self { $($f),* })
            }
        }
    };
}

wire_struct! {
    /// One direction's radio and stream settings.  24 bytes, no padding.
    pub struct StreamConfig {
        /// Local oscillator, Hz.
        pub lo_hz: u64,
        /// Complex sample rate, Hz.  TX and RX share one clock: the most
        /// recently set rate applies to both.
        pub rate_hz: u32,
        /// Analog filter bandwidth, Hz.  0 = same as the sample rate.
        pub bandwidth_hz: u32,
        /// TX: output attenuation below full power, milli-dB (0 = maximum, 10000 = -10 dB).
        /// RX: manual gain, milli-dB.
        pub gain_mdb: u32,
        /// Samples per hardware block; 0 = default (262144).  Latency and the
        /// unit of underrun fill.  Must be even.
        pub block_samples: u32,
    }
}

wire_struct! {
    /// Follows [`StreamConfig`] in the CONFIG data stage.  4 bytes.
    pub struct StreamOptions {
        pub format: u8,
        pub flags: u8,
        /// TX: blocks to buffer before the DAC starts; 0 = default (3).
        pub prebuffer_blocks: u8,
        pub reserved: u8,
    }
}

/// Size of the CONFIG data stage: [`StreamConfig`] then [`StreamOptions`].
pub const CONFIG_SIZE: usize = StreamConfig::SIZE + StreamOptions::SIZE;

wire_struct! {
    /// 40 bytes.
    pub struct Stats {
        pub tx_blocks: u64,
        /// Blocks of fill transmitted because the host was late.
        pub tx_underruns: u64,
        pub rx_blocks: u64,
        /// Blocks dropped because the host was not reading.
        pub rx_overruns: u64,
        /// bit 0: TX running, bit 1: RX running, bit 2: TX muted by the
        /// host-gone timeout.
        pub state: u32,
        /// Transceiver die temperature, milli-degrees C.
        pub temperature_mc: i32,
    }
}

wire_struct! {
    /// 8 bytes, followed by a UTF-8 firmware version string (not NUL terminated)
    /// filling the rest of the transfer; ask for 64 bytes.
    pub struct Info {
        /// `b"PMP1"` as a little-endian u32.
        pub magic: u32,
        pub protocol_version: u16,
        /// Bitmask of supported [`Format`]s, bit n = format n.
        pub formats: u16,
    }
}

pub const INFO_MAGIC: u32 = u32::from_le_bytes(*b"PMP1");

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn layouts_have_no_padding() {
        assert_eq!(StreamConfig::SIZE, 24);
        assert_eq!(StreamOptions::SIZE, 4);
        assert_eq!(Stats::SIZE, 40);
        assert_eq!(Info::SIZE, 8);
        let c = StreamConfig { lo_hz: 471_250_000, rate_hz: 6_797_203, bandwidth_hz: 0, gain_mdb: 10_000, block_samples: 226_800 };
        assert_eq!(StreamConfig::from_bytes(&c.to_bytes()), Some(c));
    }
}
