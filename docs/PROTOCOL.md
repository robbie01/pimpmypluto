# pmpstream — USB protocol, version 1

How a host talks to a pimpmypluto radio over USB. It is meant to be usable
from anything that can do control and bulk transfers — libusb, nusb, WebUSB,
PyUSB — in an afternoon. The reference implementation is the Rust crate
`crates/pmpstream` (host) and `crates/sdrd` (device); the constants and struct
layouts both compile against are in `crates/pmp-proto`.

Everything is little-endian. There are no variable-length messages and no
negotiation: a handful of fixed structs on the control endpoint, raw samples
on two bulk endpoints.

## Finding the device

| | |
|---|---|
| VID:PID | `1209:0001` (pid.codes *test* PID — will change before anyone else runs this) |
| Interface | class `0xFF`, subclass `0x50`, protocol `0x01`, string "pmpstream" |
| Endpoints | one bulk OUT (TX samples), one bulk IN (RX samples) |

The device is composite: a CDC-NCM network function comes first, so **find the
interface by its class triple, not by number**, and take the endpoint addresses
from its descriptors. The USB serial number identifies the unit (and is the
same one the board reports under stock firmware).

The network function is for bring-up and debugging (IPv6 link-local, mDNS name
`pluto.local`, ssh). Nothing in this protocol goes over it.

## Control requests

Vendor requests to the **device** recipient: `bmRequestType` `0x40` (OUT) or
`0xC0` (IN), `wIndex` 0. Use a timeout of several seconds — retuning runs
transceiver calibrations.

| bRequest | dir | wValue | data | |
|---|---|---|---|---|
| `0x01` GET_INFO | IN | 0 | `Info` + version string | ask for 64 bytes |
| `0x10` CONFIG | OUT | direction | `StreamConfig` + `StreamOptions` (28 bytes) | stops that direction first |
| `0x10` CONFIG | IN | direction | same 28 bytes | STALLs if no valid config |
| `0x20` START | OUT | direction mask | — | STALLs if a direction has no valid config, or the device cannot stream |
| `0x21` STOP | OUT | direction mask | — | TX: RF off immediately |
| `0x30` GET_STATS | IN | 0 | `Stats` (40 bytes) | |

Directions: `TX = 1`, `RX = 2`; masks OR them.

**A rejected CONFIG does not fail.** A control OUT cannot be refused after its
data stage, so an out-of-range frequency or rate is reported by the device
having *no* configuration afterwards: read it back with CONFIG IN and compare.
(The `pmpstream` crate does this for you.)

```c
struct StreamConfig {        // 24 bytes
    uint64_t lo_hz;          // local oscillator
    uint32_t rate_hz;        // complex sample rate; TX and RX share one clock,
                             //   the most recently set rate applies to both
    uint32_t bandwidth_hz;   // analog filter; 0 = same as rate
    uint32_t gain_mdb;       // TX: attenuation below full power (0 = max, 10000 = -10 dB)
                             // RX: manual gain
    uint32_t block_samples;  // hardware block size; 0 = 262144.  Must be even.
};
struct StreamOptions {       // 4 bytes
    uint8_t format;          // see below
    uint8_t flags;           // bit 0 TX_REPEAT_ON_UNDERRUN, bit 1 RX_AGC
    uint8_t prebuffer_blocks;// TX: blocks buffered before the DAC starts; 0 = 3, max 6
    uint8_t reserved;
};
struct Stats {               // 40 bytes
    uint64_t tx_blocks, tx_underruns, rx_blocks, rx_overruns;
    uint32_t state;          // bit 0 TX running, bit 1 RX running, bit 2 TX muted (host gone)
    int32_t  temperature_mc; // transceiver die, milli-degrees C
};
struct Info {                // 8 bytes, then a UTF-8 firmware version string
    uint32_t magic;          // "PMP1"
    uint16_t protocol_version;
    uint16_t formats;        // bit n set = format n supported
};
```

`block_samples` is the unit of latency (a block is handed to the DAC whole) and
of underrun fill. Set it to one frame of a periodic signal together with
`TX_REPEAT_ON_UNDERRUN` and a late host produces a freeze-frame instead of a
dropout — e.g. 226800 for one NTSC frame at 432 samples per line.

## Sample formats

All complex, I then Q, and all **full scale at the type's range**: +127, +2047
and +32767 are the same amplitude.

| format | | bytes/sample | max rate, TX | max rate, RX |
|---|---|---|---|---|
| 0 `ci16` | int16 I, int16 Q | 4 | 8 MS/s | 8 MS/s |
| 1 `ci8` | int8 I, int8 Q | 2 | 16 MS/s | 12 MS/s |
| 2 `ci12` | 12-bit I, Q packed: `I[7:0]`, `Q[3:0]<<4 \| I[11:8]`, `Q[11:4]` | 3 | 11.5 MS/s | 11 MS/s |

The converters are 12-bit, so `ci12` is lossless and `ci16` carries four
padding bits. The rates are measured, one direction at a time, with no
dropped samples (about 33.5 MB/s on the wire for TX; RX is limited by the
device's CPU for `ci8`). **A CONFIG above its cap is rejected** -- read the
configuration back to see it -- rather than accepted and then dropped from.
Full duplex shares the bus.

## Streaming

Bulk pipes carry raw samples and nothing else. **TX is paced by the device:**
write as fast as the endpoint accepts; it NAKs when its buffers are full, which
locks the producer to the DAC clock. There is no need to time anything on the
host.

### Start-of-stream marker

```
50 4D 50 53 54 52 4D 31      "PMPSTRM1"
```

After START, each direction begins with this marker **as a transfer of its
own** (so it ends in a short packet):

* **TX:** the host sends the marker on the OUT endpoint, then samples.
  The device discards everything up to and including the first short
  transfer that ends with the marker.
* **RX:** the device sends the marker on the IN endpoint, then samples.
  The host discards everything up to and including the first short transfer
  that ends with the marker. Give up after a few seconds without one: the
  marker precedes the first sample, so its absence means no stream is coming.

This is not decoration. A bulk pipe has no framing, and a transfer abandoned
by an earlier stream is still on the endpoint when the next one starts: a host
that exited mid-write leaves the device's pending read partly filled; a host
that stopped reading leaves a device write partly sent. Those bytes would
lead the new stream. They are always a multiple of the packet size — invisible
to 2- and 4-byte samples, a permanent misalignment for 3-byte ones. The marker
makes the start of a stream something both ends can verify rather than assume.

With libusb: after START, `libusb_bulk_transfer(out, marker, 8)`; for RX,
read transfers until one comes back shorter than you asked for with those
8 bytes at its end.

### Underrun, overrun, and a host that goes away

The DAC is never starved and never stalls. If the host is late the device
transmits fill — silence, or the last block with `TX_REPEAT_ON_UNDERRUN` — and
counts it in `tx_underruns`. If nothing but fill has gone out for 2 s the
device decides the host is gone and **mutes the transmitter** (state bit 2); it
unmutes by itself when samples arrive again. If the host stops reading RX, the
device drops whole blocks and counts them in `rx_overruns`; the stream resumes
sample-aligned.

A minimal transmitter therefore needs no supervision logic at all:

```
CONFIG(TX, ...); check it; START(TX); write marker; write samples forever
```

## Not in version 1

* Timestamps / timed transmit. The stock bitstream cannot stamp or gate
  samples, so framing them (VITA 49) would be header overhead with nothing
  behind it. The format byte and protocol version leave room for a framed mode
  once there is gateware to honour it.
* Sample rates below ~2.1 MS/s (needs the transceiver's FIR decimators loaded).
* WinUSB/WebUSB descriptors.
