# Do we gain anything from custom gateware?

Written 2026-09-21 as a decision input, before any gateware work. Short
answer: **not for the things you want to do first; yes for two specific things
later; and one cheap experiment should come before committing either way.**

Confidence markers: **[measured]** on this board, **[source]** read in code or
registers, **[recall]** from memory and not verified — treat those as leads.

## What the stock bitstream + our firmware already does

* Full-duplex 16-bit I/Q DMA to and from DDR, driven by mainline `axi-dmac`
  and `pmpstream`; any block size from 2 samples up; never-starve semantics in
  the kernel. **[measured]**
* ci8 / ci12 / ci16 on the wire, converted on the CPU at 27–29 MS/s per core
  **[measured]** — several times what USB 2.0 can carry. Moving format
  conversion into the FPGA would save CPU we are not short of.
* Sustained USB throughput: 33.5 MB/s with queued AIO **[measured]** (24.5
  with synchronous I/O) -- 16 MS/s of ci8, 8 MS/s of ci16.
* An ADI ×8 FIR interpolator/decimator pair in the datapath **[source: stock
  device tree]**, unused so far; it is what gives sample rates below ~2 MS/s.
* Capture or playback of bursts at up to the converter's full 61.44 MS/s into
  and out of DDR (the rings are 40 MiB each: 0.17 s of ci16 at full rate).
  Only *continuous* streaming is limited by USB.

## Your applications, one by one

| application | needs | stock bitstream enough? |
|---|---|---|
| NTSC (6.8 MS/s; DSB 9.06; composite-rate 13.6) | 13.6 / 18 / 27 MB/s of ci8 | **Yes.** All three fit in the measured 33.5 MB/s. |
| ATSC 8VSB | 10.76 MS/s complex, ~21.5 MB/s as ci8 | **Yes.** ci8's ~40 dB of usable SNR is ample for 8VSB. |
| DVB-T / DVB-S | ≤ ~9 MS/s | **Yes.** It was the crashes, not the bitstream. |
| Scripted DSP | CPU time | **Yes** — two A9 cores with NEON, mostly idle. 80 DSP48s would not change the picture much. |
| Wideband analysis, burst | 61.44 MS/s into DDR | **Yes**, for captures up to the ring size. |
| Wideband analysis, continuous survey | data reduction *before* USB | **No.** This is gateware's clearest win: FFT/PSD, peak-hold or channelised power computed in the PL, shipping spectra instead of samples. |
| HaLow ACK within SIFS (160 µs) | deterministic RX→TX turnaround | **Probably not** — see below. |

## HaLow is the hard one, and gateware may not be sufficient either

The ACK decision needs the received frame demodulated and its FCS checked
inside 160 µs. Three ways to get there:

1. **Host in the loop** — impossible; USB alone costs milliseconds.
2. **CPU1 bare-metal + stock bitstream.** Poll the DMA with tiny blocks,
   demodulate 1 MHz OFDM in NEON, fire a pre-built ACK. The arithmetic is
   small (11ah at 1 MHz is ~150 kb/s–4 Mb/s), but the stock path gives no
   sample-accurate TX trigger and no hardware RX/TX (ENSM) switching, so
   timing would ride on software polling. **Unknown; measurable in a day** —
   and now possible at all, since the second core works again.
3. **PHY in gateware.** The natural design, but openwifi, the one open
   802.11 OFDM PHY for this transceiver family, targets the Zynq 7020 and up,
   not the 7010 **[recall]**. An 11ah PHY is narrower-band than 11a/g, so it
   might fit; nobody should assume it does.

So for HaLow custom gateware is *necessary-ish* (at minimum: timestamps, a
timed-TX trigger, ENSM control — small blocks) and not obviously *sufficient*.
Newer ADI Pluto bitstreams include a TDD timing engine **[recall]**. This
vendor's build does not appear to: its device tree has no such node
**[source]**, and a read at 0x7C440000, where ADI's design puts it **[recall]**,
gets no response at all -- the AXI bus hangs, which is what an address with no
slave does in this design (the DMA and converter cores either side of it read
back fine) **[measured]**. It cost a watchdog reset to find out, so other
addresses were not tried.

## What it would cost

* **Pinout — solved (2026-09-21).** The board is the AliExpress "PlutoSDR
  Nano"; F5OEO's tezuka_fw supports it, and its HDL fork carries a complete
  XDC, top level and PS7 configuration for it (`vendor/tezuka-nano/`), derived
  from the seller's own pin table. It matches everything measured on this unit
  that can be checked without building a bitstream. The one-to-two weeks of
  bitstream archaeology estimated here earlier is gone; what remains is
  confirming it on hardware, which a first bitstream does anyway.
* **Toolchain.** yosys + nextpnr-xilinx handles the part **[source: prjxray
  DB]**, but PS7 + AXI HP + a source-synchronous 61.44 MHz CMOS interface is
  a demanding first project for it. Doing first bring-up in Vivado on your
  Linux box and porting to the open flow afterwards separates "unknown
  pinout" from "immature toolchain" — two unknowns I would not debug at once.
* **Everything the vendor got right for free**: the AD9363 interface timing
  that works today, the DMA, the FIR cores. A from-scratch design re-earns all
  of it, on a board that has already shown it is electrically touchy.
* **FPGA headroom** on the 7010 (17.6 k LUTs, 80 DSP48, 2.1 Mb BRAM) next to
  the existing design is unknown to me; ADI's Pluto design is reputed to be
  fairly full **[recall]**.

## Recommendation

1. **Don't start gateware now.** NTSC, ATSC, DVB, scripted DSP and burst
   capture are all served by what runs today; finishing AIO and the Rust
   driver is worth more to those than any PL work.
2. ~~Two cheap experiments~~ Both done: no TDD core (above), and the
   turnaround measurement below.
3. **If wideband survey becomes a priority**, that is the case for gateware
   that stands on its own — and it can be an *addition* to the vendor design's
   datapath idea (a PSD block beside the DMA) rather than a replacement, which
   keeps the risk down.
4. ~~Find out which clone this is.~~ Done: PlutoSDR Nano, pinout public. This
   lowers the *cost* of gateware a great deal; it does not change what gateware
   is *for* (points 1–3). It also opens a cheaper middle path: tezuka's
   maia-hdl project is a complete, working Vivado design for this exact board,
   so an experiment such as "add timestamps and a timed-TX trigger" can start
   from a design known to bring the radio up instead of from an empty one.

## Measured: software RX→TX turnaround through the stock bitstream (2026-09-21)

`sdrd turnaround-test`: the board hears itself through TX→RX leakage. It
sends a burst, and when the RX block holding the burst's *end* arrives,
userspace commits a reply burst; the reply's onset is found in the same RX
stream, so the result is counted in converter samples and is exact. Linux
userspace (SCHED_FIFO, CPU1) through the Rust `pmpstream`; 4 MS/s; 1000 trials
per row; "depth" is how many transfers the driver keeps queued in the TX DMA
(a `pmp_setup` field since this test; the default is 4). **[measured]**

| depth | block | min | median | p99 | max µs | no reply | CPU0 (DMA completions) |
|---|---|---|---|---|---|---|---|
| 4 | 256 (64 µs) | 320 | 320 | 384 | 384 | 0 | 10 % |
| 4 | 128 (32 µs) | 160 | 160 | 192 | 192 | 0 | 43 % |
| 4 | 64 (16 µs) | 92 | 100 | 120 | 126 | **1.8 %** | 96 % |
| 2 | 256 | 192 | 192 | 256 | 256 | 0 | 5 % |
| **2** | **128** | **96** | **128** | **128** | **160** | **0** | **49 %** |
| 2 | 64 | 68 | 106 | 141 | 152 | 0 | 92 % |
| 1 | 128 | 53 | 107 | 151 | 186 | 0 | 51 % |

(32-sample blocks do not work at all: at 125 000 completions a second the TX
path never gets a burst out. Depth 1 lets the DMA go idle between blocks, which
is why it is both the fastest and the least predictable.)

What this says:

* **Speed is not the problem I expected it to be.** The floor is
  (depth + 1) blocks, and with depth 2 and 128-sample blocks a reply starts
  96–160 µs after the frame ends, every time, with half of one core left over
  and the other core untouched. A bare-metal CPU1 would shave the ~20 µs of
  wake-up-and-scan, not the block arithmetic, so it is not worth building for
  this.
* **Precision is the problem.** The reply starts on a TX block boundary, and
  the TX block grid has no fixed relation to when the frame ended, so the reply
  time is spread over a block or two (±32 µs here). 802.11 wants the ACK at
  SIFS to within a small fraction of a slot (S1G: SIFS 160 µs, slot 52 µs
  **[recall]**). Landing "somewhere in 96–160 µs" is early as often as not,
  and padding to be safe spreads into the next slot.
* **Not included:** demodulating the end of the frame and checking its FCS
  before deciding to ACK. The test reacts to energy falling. A real receiver
  can decode symbol-by-symbol as blocks arrive, so most of that cost overlaps
  the wait -- but it has not been shown.

So for HaLow the honest position is: the stock bitstream is *fast enough* and
*not precise enough*. Two ways to buy precision:

1. **A timed-TX trigger in gateware** -- sample counter, "start this buffer at
   count N". Small, and with tezuka's working design for this exact board as
   the starting point, no longer a from-scratch project. This is the robust
   answer and the first gateware feature that pays for itself.
2. **Software only:** calibrate the RX↔TX stream offset once per session with
   the same leakage trick this test uses, have the driver report which TX slot
   a commit landed in, and place the ACK at a sample offset inside the block.
   It only holds while neither DMA ever runs dry, and it is unproven. Worth a
   day before committing to (1); not something to build a product on unseen.
