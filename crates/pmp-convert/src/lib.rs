//! Sample format conversion between the wire (pmp_proto::Format) and the
//! converters' native layout: interleaved I,Q int16.
//!
//! Scaling convention: every wire format is full scale at its type's range.
//! The DAC takes the top 12 bits of an int16; the ADC delivers sign-extended
//! 12-bit values.
//!
//! This runs on the CPU only because the stock bitstream has no unpacker;
//! it is the first thing that would move into gateware.

use pmp_proto::Format;

/// Wire bytes -> native samples, tolerant of samples split across USB reads.
pub struct Unpacker {
    format: Format,
    carry: [u8; 4],
    ncarry: usize,
}

#[inline(always)]
fn sext12(v: u16) -> i16 {
    ((v << 4) as i16) >> 4
}

impl Unpacker {
    pub fn new(format: Format) -> Self {
        Unpacker { format, carry: [0; 4], ncarry: 0 }
    }

    #[inline(always)]
    fn one(format: Format, b: &[u8]) -> (i16, i16) {
        match format {
            Format::Ci16 => (i16::from_le_bytes([b[0], b[1]]), i16::from_le_bytes([b[2], b[3]])),
            Format::Ci8 => (((b[0] as i8) as i16) << 8, ((b[1] as i8) as i16) << 8),
            Format::Ci12 => {
                let i = b[0] as u16 | ((b[1] as u16 & 0x0f) << 8);
                let q = (b[1] as u16 >> 4) | ((b[2] as u16) << 4);
                (sext12(i) << 4, sext12(q) << 4)
            }
        }
    }

    /// Convert as much of `input` as fits in `out` (interleaved I,Q).
    /// Returns (bytes consumed, i16 values written).
    pub fn feed(&mut self, input: &[u8], out: &mut [i16]) -> (usize, usize) {
        let bps = self.format.bytes_per_sample();
        let (mut used, mut made) = (0, 0);

        // finish a sample left incomplete by the previous read
        if self.ncarry > 0 {
            if out.len() < 2 {
                return (0, 0);
            }
            let take = (bps - self.ncarry).min(input.len());
            self.carry[self.ncarry..self.ncarry + take].copy_from_slice(&input[..take]);
            self.ncarry += take;
            used = take;
            if self.ncarry < bps {
                return (used, 0);
            }
            (out[0], out[1]) = Self::one(self.format, &self.carry);
            self.ncarry = 0;
            made = 2;
        }

        let n = ((input.len() - used) / bps).min((out.len() - made) / 2);
        let (src, dst) = (&input[used..used + n * bps], &mut out[made..made + n * 2]);
        match self.format {
            Format::Ci16 => {
                for (d, s) in dst.iter_mut().zip(src.chunks_exact(2)) {
                    *d = i16::from_le_bytes([s[0], s[1]]);
                }
            }
            Format::Ci8 => {
                for (d, s) in dst.iter_mut().zip(src) {
                    *d = ((*s as i8) as i16) << 8;
                }
            }
            Format::Ci12 => {
                for (d, s) in dst.chunks_exact_mut(2).zip(src.chunks_exact(3)) {
                    (d[0], d[1]) = Self::one(Format::Ci12, s);
                }
            }
        }
        used += n * bps;
        made += n * 2;

        // A trailing partial sample is ours to keep -- unless the output filled
        // up first, in which case the caller comes back with the same bytes.
        let rest = input.len() - used;
        if rest > 0 && rest < bps && made < out.len() {
            self.carry[..rest].copy_from_slice(&input[used..]);
            self.ncarry = rest;
            used += rest;
        }
        (used, made)
    }
}

/// Native samples (12-bit, sign-extended) -> wire bytes.  `out` is cleared first.
pub fn pack(format: Format, samples: &[i16], out: &mut Vec<u8>) {
    out.clear();
    match format {
        Format::Ci16 => out.extend(samples.iter().flat_map(|v| (v << 4).to_le_bytes())),
        Format::Ci8 => out.extend(samples.iter().map(|v| (v >> 4) as i8 as u8)),
        Format::Ci12 => {
            for iq in samples.chunks_exact(2) {
                let (i, q) = (iq[0] as u16 & 0xfff, iq[1] as u16 & 0xfff);
                out.extend_from_slice(&[i as u8, (i >> 8) as u8 | ((q & 0xf) << 4) as u8, (q >> 4) as u8]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(format: Format, split: usize) {
        // native 12-bit samples -> wire -> (DAC-scaled) native, across a split read
        let native: Vec<i16> = (0..64).map(|n| ((n * 131) % 4096 - 2048) as i16).collect();
        let mut wire = Vec::new();
        pack(format, &native, &mut wire);
        let mut u = Unpacker::new(format);
        let mut out = vec![0i16; native.len()];
        let mut made = 0;
        for part in [&wire[..split], &wire[split..]] {
            let mut off = 0;
            while off < part.len() {
                let (c, m) = u.feed(&part[off..], &mut out[made..]);
                assert!(c > 0 || m > 0, "no progress");
                off += c;
                made += m;
            }
        }
        assert_eq!(made, native.len());
        for (a, b) in native.iter().zip(&out) {
            let expect = match format {
                Format::Ci8 => ((a >> 4) as i8 as i16) << 8,
                _ => a << 4,
            };
            assert_eq!(*b, expect);
        }
    }

    #[test]
    fn formats_roundtrip_across_split_reads() {
        for f in [Format::Ci16, Format::Ci8, Format::Ci12] {
            for split in [0, 1, 2, 3, 5, 7, 64] {
                roundtrip(f, split);
            }
        }
    }

    /// The shape of the real thing: arbitrary USB read sizes feeding
    /// fixed-size blocks, with neither a multiple of the sample size.
    #[test]
    fn arbitrary_chunking_into_blocks_is_lossless() {
        let mut seed = 12345u32;
        let mut rnd = move |m: usize| {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            (seed >> 8) as usize % m
        };
        for f in [Format::Ci16, Format::Ci8, Format::Ci12] {
            let native: Vec<i16> = (0..20000).map(|n| ((n * 37) % 4096 - 2048) as i16).collect();
            let mut wire = Vec::new();
            pack(f, &native, &mut wire);
            let mut u = Unpacker::new(f);
            let (mut got, mut block, mut filled) = (Vec::new(), vec![0i16; 2 * (1 + rnd(50))], 0);
            let mut at = 0;
            while at < wire.len() {
                let end = (at + 1 + rnd(97)).min(wire.len());
                let mut off = at;
                while off < end {
                    let (c, m) = u.feed(&wire[off..end], &mut block[filled..]);
                    assert!(c > 0 || m > 0 || filled == block.len());
                    off += c;
                    filled += m;
                    if filled == block.len() {
                        got.extend_from_slice(&block);
                        block = vec![0i16; 2 * (1 + rnd(50))];
                        filled = 0;
                    }
                }
                at = end;
            }
            got.extend_from_slice(&block[..filled]);
            let expect: Vec<i16> = native.iter().map(|a| match f {
                Format::Ci8 => ((a >> 4) as i8 as i16) << 8,
                _ => a << 4,
            }).collect();
            assert_eq!(got.len(), expect.len(), "{f:?}: sample count");
            assert!(got == expect, "{f:?}: first mismatch at {:?}", got.iter().zip(&expect).position(|(a, b)| a != b));
        }
    }

    /// Exactly what sdrd does on TX: 256 KiB USB buffers into 1 MiB blocks.
    #[test]
    fn device_tx_geometry_preserves_a_tone() {
        for f in [Format::Ci16, Format::Ci8, Format::Ci12] {
            let n = 1 << 20;
            let native: Vec<i16> = (0..n).flat_map(|k| {
                let ph = std::f64::consts::TAU * (k % 8) as f64 / 8.0;
                [(ph.cos() * 1023.0) as i16, (ph.sin() * 1023.0) as i16]
            }).collect();
            let mut wire = Vec::new();
            pack(f, &native, &mut wire);
            let mut u = Unpacker::new(f);
            let mut block = vec![0i16; 1 << 19];
            let (mut filled, mut got) = (0, Vec::new());
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
            got.extend_from_slice(&block[..filled]);
            assert_eq!(got.len(), native.len(), "{f:?}");
            let bad = got.iter().zip(&native).position(|(g, n)| match f {
                Format::Ci8 => *g != ((n >> 4) as i8 as i16) << 8,
                _ => *g != n << 4,
            });
            assert_eq!(bad, None, "{f:?}: first wrong value at index {bad:?}");
        }
    }

    #[test]
    fn stops_at_full_output_without_losing_bytes() {
        let wire: Vec<u8> = (0..40).collect();
        let mut u = Unpacker::new(Format::Ci16);
        let mut a = [0i16; 6];
        let (c, m) = u.feed(&wire, &mut a);
        assert_eq!((c, m), (12, 6));
        let mut b = [0i16; 32];
        let (c2, m2) = u.feed(&wire[c..], &mut b);
        assert_eq!((c2, m2), (28, 14));
        assert_eq!(b[0], i16::from_le_bytes([12, 13]));
    }
}
