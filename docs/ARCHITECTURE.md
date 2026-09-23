# pimpmypluto — architecture

Replacement firmware and gateware for a PlutoSDR-compatible board.

## The board (measured, not assumed)

| | |
|---|---|
| SoC | Zynq XC7Z010, **CLG400 package** (genuine Plutos are CLG225 — pinout differs, ADI's XDC does not apply) |
| CPU | 2× Cortex-A9 @ 667 MHz, both in use (the vendor runs one — see "DDR address bus" for why) |
| RAM / flash | 512 MB DDR3 (one 16-bit part; address bus needs slower edges than the vendor FSBL sets — fixed in our kernel) / 32 MB W25Q256 |
| RF | AD9363, 40 MHz reference, SPI0, RESETB = EMIO GPIO 67, EN_AGC = EMIO GPIO 66 |
| USB | ChipIdea HS device, ULPI PHY, PHY reset = MIO 46 |
| Identity | sold on AliExpress as **"PlutoSDR Nano"** (Zynq7010 + AD9363, USB-C, no SD, no Ethernet). F5OEO's tezuka_fw supports it as board `nano`; its HDL fork has the full PL pinout — reference copy and provenance in `vendor/tezuka-nano/` |
| Stock fw | vendor "dirty" build of ADI v0.39, Oct 2025, Vivado 2023.2 bitstream — not downloadable anywhere; the only copy is the flash backup |

## Principles

1. **U-Boot is never touched.** The first 1 MB of flash (FSBL + U-Boot) is the
   restore path; Linux mounts it read-only. Everything else can always be
   re-flashed over USB DFU. See [RESTORE.md](RESTORE.md).
2. **Develop from RAM.** `tools/ramboot.sh image.itb` boots an image over DFU
   without writing flash; a power cycle returns to whatever is installed.
   Round trip is ~20 s.
3. **Borrow the hard parts, own the plumbing.**

| Borrowed from ADI | Ours |
|---|---|
| U-Boot + DFU (unchanged, in flash) | mainline LTS kernel, minimal config (`os/kernel/`) |
| AD9361 driver: `no-OS` `ad9361*.c`, run in userspace over spidev | sample datapath: USB ⇄ DMA, no IIO, no libiio |
| `axi_adc_core` / `axi_dac_core` register drivers (no-OS) | USB gadget: NCM + vendor bulk interface |
| stock bitstream, until ours replaces it | 1 MB userland: busybox, dropbear, mdnsd, sdrd |
| HDL as reference for the AD9363 CMOS interface | gateware in Amaranth, open toolchain (yosys + nextpnr-xilinx) |

## Why not seL4 / why Linux

The Cortex-A9 has no virtualization extensions, so seL4 cannot host an
unmodified Linux on this part. It is also unnecessary: the crashes in stock
firmware come from ADI's IIO DMA buffer stack (a starved TX DMA wedges
forever), not from Linux. The plan is Linux on CPU0 for USB/network/control
and, when HaLow needs it, a bare-metal or Zephyr image on CPU1 via remoteproc
with timing-critical work (TX-at-timestamp, SIFS turnaround = 160 µs for
802.11ah) done in gateware.

## Datapath

Stock: USB → NCM → TCP → userspace → copy → IIO buffer → DMA. The CPU touches
every sample three times and iperf alone costs 69 % of the one core in use;
the usable ceiling was ~145 Mbit/s.

As built (M1d), on the stock bitstream:

```
host ─ USB bulk ─► FunctionFS ─► sdrd: unpack ci8/ci12/ci16 ─► pmpstream ring ─► axi_dmac ─► DAC
host ◄─ USB bulk ─ FunctionFS ◄─ sdrd: pack                 ◄─ pmpstream ring ◄─ axi_dmac ◄─ ADC
```

* **`os/kernel/pmpstream-rs`** (Rust, plus a 60-line C shim; see "The Rust
  port of pmpstream". The C original, `os/kernel/pmpstream`, is kept as the
  reference and fallback: `make PMP_RUST=`): a dmaengine client of mainline's
  `axi-dmac`. Each direction is a ring of equal blocks in one DMA-coherent
  allocation made at probe and never freed, mmap()ed into sdrd. The rule it
  exists to enforce: once started the converter is *always* fed. The next
  block is queued from the DMA completion callback — real if userspace has one
  ready, fill (silence, or the last block repeated) if not — so no userspace
  delay can starve the DAC, and an underrun is a counter, not a wedge.
* **`crates/sdrd`** (Rust): the radio (ADI's no-OS AD9361 driver over FFI,
  spidev + UIO), the USB function (FunctionFS), format conversion, and the
  policy that used to be a supervisor script on the host: mute the transmitter
  when the host goes away, unmute when it returns, stop feeding the hardware
  watchdog if the datapath wedges. USB I/O and conversion run in separate
  threads, and endpoint I/O is Linux native AIO with 16 requests queued, so a
  request is always posted on the endpoint.
* Format conversion is on the CPU only because the stock bitstream takes
  nothing but 16-bit I/Q. It costs little (27–29 MS/s per core, measured with
  `sdrd convert-test`); it is still the first thing that would move into
  gateware, along with timestamps.
* The zero-copy path (USB request buffers = DMA ring) is not built: with the
  CPU 85 % idle at full USB rate it would buy nothing yet.

## Host interface

* **Network:** USB NCM, IPv6 link-local only. No DHCP, no addresses to collide.
  Discovery by mDNS/DNS-SD (`pluto.local`, `pluto-<serial4>.local`,
  `_pimpmypluto._tcp`), or `ping6 ff02::1%iface`.
* **Raw USB — pmpstream:** one vendor-class interface, two bulk endpoints, six
  control requests. Specified in [PROTOCOL.md](PROTOCOL.md); host side is the
  `pmpstream` crate (nusb: pure Rust, nothing to install on macOS) and the
  `pmp` CLI. This is the interface applications use.
* **Framing:** none in protocol v1 — raw samples plus a start-of-stream
  marker. VITA 49 was the plan; it is deferred until there is gateware that can
  timestamp or gate samples, because without that the headers carry nothing.

## Milestones

- [x] M0 flash backup, restore procedure, RAM-boot dev loop
- [x] M1a our userland on the stock kernel: NCM/IPv6-LL, mDNS, ssh with a host key that survives reboots
- [x] M1b mainline 6.18 LTS kernel, built natively on macOS: both CPUs, NCM, spidev, UIO, FPGA manager, watchdog, ramoops
- [x] M1c AD9363 brought up from Rust through ADI's no-OS driver; DDS tone seen by the board's own receiver at +38 dB (`sdrd radio-test`)
- [~] M1d pmpstream (stable under soak since the DDR fix; kernel built with GCC by default; Rust-for-Linux verified to build on this host for ARMv7, incl. platform and misc-device samples): kernel rings, sdrd, protocol, host crate, `pmp` CLI; ci8/ci12/ci16 verified end to end over USB in both directions (`pmp loopback`); plutotv transmitting through it with no responder and no supervisor. TX and RX both survive long soaks. Flashed 2026-09-21 (`tools/flash.sh`; back out with `tools/restore-stock.sh`). Since then: AIO endpoints (33.5 MB/s), mainline spi-nor working on this controller, stock-identical serial number and MACs. The Rust port of the kernel module passes the same 105-session soak as the C one, SIGKILL of `sdrd` mid-stream, and unload/reload; the Rust one is the default build.
- [ ] (decision input for M2: [GATEWARE-EVAL.md](GATEWARE-EVAL.md) — recommendation: not yet)
- [ ] M2 gateware: recover clone pinout from the stock bitstream, AD9363 CMOS interface, DMA ring, format unpackers
- [ ] M3 scripted DSP, CPU1 real-time image, HaLow timing

## DDR address bus: why this board crashed

**Root cause: on this clone the DRAM occasionally mis-samples address line A7
during a write, because the vendor's boot firmware drives the DDR address pins
with edges too fast for the board. Fixed from the kernel; see below.** The write burst
lands 256 bytes from where it was aimed; the intended location keeps its old
contents and the victim gets someone else's data. Under any kernel. This is
almost certainly also why stock firmware crashes under heavy streaming.

How it was found (2026-09-20), after two days of blaming software:

* `tools/memtest.sh` -- one pinned `memstress` process per core, 96 MiB each;
  no USB traffic, no radio, no drivers -- shows corrupted memory in two minutes.
  **ADI's own 6.1 kernel fails the same way** once CPU1 is onlined
  (`echo 1 > /sys/devices/system/cpu/cpu1/online` in `build/rescue.itb`).
* The errors are *lost writes* (memory still holds the previous pass's value),
  exactly **16 bytes** long -- one DDR burst: 8 transfers on this board's
  single 16-bit DDR3 part -- never at the same address twice, scattered over
  all of RAM.
* Their offsets have period 0x800 (the DDR column address space) and start
  only at columns with **A9=1, A5=1, A6=0**; 40 of 54 had a partner burst
  exactly 0x100 bytes away (**column bit A7**) holding foreign data.
* It scales with how hard and how *widely* memory is driven: two cores on
  96 MiB buffers, tens of errors a minute; two cores on 8 MiB, none seen;
  one core alone, none seen in ~500 passes -- but one core *plus* DMA plus a
  memory stress still killed a single-CPU kernel in four minutes.

Ruled out by experiment: every kernel-side suspect (ADI's kernel fails too);
compiler; page migration; bad DRAM cells; CPU clock margin (identical at
444 MHz); A9 prefetch/FLZ; PL310 FLZ, early BRESP and RAM latencies; Cortex-A9
erratum 845369 and every other diagnostic-register workaround (applied and
verified per-CPU with `os/kernel/a9reg`). The vendor FSBL programs Xilinx's
stock DDR timing and I/O values; evidently they do not suit this layout.

**The fix: slow the address pins' edges.** Drive strength and slew rate of
the DDR address/command pins live in one SLCR register
(`DDRIOB_DRIVE_SLEW_ADDR`, 0xF8000B5C), which the vendor FSBL programs with
Xilinx's default, 0x0018C61C (slew 3/3). Changing it on the running system and
counting lost writes (`tools/ddr-sweep.sh`, two cores, 100 s each):

| slew P/N | lost-write errors |
|---|---|
| 3 / 3 (vendor) | 128 |
| 1 / 1 | 24 |
| **0 / 0** | **0** |
| 6 / 6 | board died |
| 31 / 31 | 36 |

Alternating the two settings on one boot gave 0, 0, 0, 0 errors at slew 0 and
16, 8, 16 at the vendor value. The edges are simply too fast for this layout.

Drive strength, swept at slew 0, has a bounded clean window -- P=0x0E dies,
0x11 gives 312 errors, **0x13 to 0x1E are clean**, 0x20 and above die (stronger
drive fails the same way faster slew does). The vendor's 0x1C sits two steps
under the upper cliff, so the board setting uses the middle of the window:
**0x00000518** (drive P/N 0x18/0x0A, slew 0/0). That window was measured at
room temperature on one unit; it is margin, not a guarantee. `tools/memtest.sh`
is the two-minute check to rerun if this board ever misbehaves again.

It is applied by a small kernel patch
(`os/kernel/patches/0002`, in `zynq_early_slcr_init()`: before SMP bring-up,
before the initramfs is unpacked) from a device-tree property of this board:

```dts
&slcr { pmp,ddriob-drive-slew-addr = <0x00000518>; };
```

With it, on **two cores**, GCC-built 6.18.52:

* two-core memory stress, 10 min: 1230 passes, **0 errors**
  (vendor setting: more than one lost write per second);
* 210 full-duplex streaming sessions with a memory verifier running on the
  board throughout: all 210 completed, verifier 2765 passes / **0 errors**,
  kernel log clean, board up 37 minutes and still going. No earlier build
  survived that test, on one core or two; most died inside 25 sessions.

Caveats:

* **U-Boot now applies the same value** from its `preboot` environment
  variable (`tools/flash.sh` writes the environment; holding the button skips
  it, so the DFU restore path cannot be locked out). What is left at the
  vendor setting is the FSBL and U-Boot up to `preboot` -- short,
  single-master, never seen to fail. A truly complete fix belongs in the FSBL,
  in the 1 MB of flash this project does not write. Owner's call.
* **Stock firmware still has the problem.** If you ever run it under heavy
  streaming again, `devmem 0xF8000008 32 0xDF0D; devmem 0xF8000B5C 32 0x00000518`
  early in its boot is the same fix.
* Only the address pins were changed. Data, DQS and clock pins are at the
  vendor's values and showed no errors; they were not swept.
* The second core is back on. With it, the option of a bare-metal/RTOS CPU1
  for HaLow timing is open again.

### Earlier conclusions that were wrong

Written down with confidence at the time, all false:

* "RX DMA sample data is overwriting kernel memory" -- the fault addresses
  that looked like IQ pairs were just small integers.
* "The trigger is FunctionFS synchronous I/O plus device-to-host USB load" --
  that combination merely drove memory hard enough to lose writes quickly.
* "The second core is the problem; one core stopped the crashes" -- one core
  only made it rarer; with the DDR fix both cores run clean.
* "Compiler and CPU count change how soon, not whether" -- true, and it should
  have pointed at hardware much sooner.
* Every defensive change made along the way (permanent rings, drain on stop,
  errata, guarded stacks, no chipidea scatter-gather, ADC gating, run-time DMA
  address assertion, dma_pool and L2 take-over patches) is harmless and none
  was fixing what it was written for. Single-TD USB reads and the
  start-of-stream marker did fix real, separate bugs.

### sdrd restart (resolved)

On the kernels that were corrupting memory, a killed `sdrd` never came back and
the watchdog reset the board. With the DDR fix it behaves as designed:
`killall sdrd` -> init logs the exit, restarts it, the radio re-initialises,
the gadget re-binds, and streaming works again within ~3 s. It was a symptom.

## Known issues

* **What is in flash** (2026-09-23): `build/pluto.itb` md5 `ad8b6bdc…` --
  kernel with all four DDR IOB groups from the device tree (data/strobe at
  slew 6/6), Rust `pmpstream`, `sdrd` with the rate caps and `--fake-rx` --
  and the U-Boot env `uboot-env-pmp2.bin` whose `ddrfix` sets address, data
  and strobe pins before the kernel. Verified by DFU readback, from
  `/dev/mtd*ro` after boot, and by the kernel's "DDR … pins" lines reading
  the new values *before* its own write. Bootloader still byte-identical to
  the backup. Validation before flashing, on this image from RAM: two-core
  memtest clean, 14 MS/s ci8 RX for an hour without a death, 105-session
  soak clean, 12 MS/s ci8 for 20 min with 0 drops.
* `tools/ramboot.sh` can wedge *stock* firmware: its password-login fallback
  runs `reboot` on stock, which on one occasion hung half-way through stock's
  shutdown (http still up, every new process hanging). Only a replug clears
  it; stock's own `config.txt` reset is not an option because it rewrites the
  U-Boot environment in flash.
* **Kernel deaths under heavy RX (2026-09-21, fixed 2026-09-23 -- the trail below is kept because every step of it was wrong until the last).** Twice in
  one day the board died while `pmp rx` ran ci8 at 16 MS/s -- above the RX
  ceiling, so the ring was overrunning continuously. Once it dropped off USB
  and never came back until replugged (no log: the watchdog did not fire, and a
  replug clears ramoops). The second time it panicked and ramoops caught it
  (`build/lastlog-rxhammer-192647.txt`): in the `usb-in` thread, inside
  `io_submit` → `arm_copy_from_user`, the CPU jumped to 0xbf028d24 -- in the
  memory of the `pmpstream` module, but past the end of its text: a junk
  function pointer, not a bug at that address. Everything else about the crash
  is ordinary (stack sane, no DMA involvement in the path). Facts so far:
  * 12 + 8 hammer runs (60 s each) at the same settings did *not* reproduce
    it, the 8 with `memstress` verifying 96 MiB the whole time: 0 errors.
    Runs at ≤ 14 MS/s never died. So it is rare and not the DDR fault coming
    back.
  * Reading the uncached ring a sample at a time (the pre-fix RX path) was
    running during the first death; the bulk-copy path during the second.
  * Suspects, unranked: the Rust `pmpstream` (this is its first day; the C
    one has more soak hours but none under continuous overrun); the AIO path
    under overrun (`io_submit` is where it died); something in FunctionFS.
    A corrupted function pointer inside module memory points at the first.
  * **The C module dies the same way** (`tools/rxhammer.sh` at 16 MS/s ci8:
    dead on its first 60 s run, silently -- the log just stops and the
    watchdog reboots). So it is not the Rust port.
  * **Overrun alone does not do it.** `tools/overrun-hammer.sh` (host reads
    at ~a quarter of the rate, so the device converts and submits flat out
    while its ring drops 3 blocks in 4) at 8 MS/s ci8: C module 20 × 60 s
    plus 31 min of total overrun, no death. So the trigger is the *rate* --
    64 MB/s of ADC DMA into DDR plus 28 MB/s of USB out of it -- not the
    overrun path in either driver.
  * **Not the DMA/DDR side.** `sdrd rx-soak` at 16 MS/s -- the ADC writing
    DDR at 64 MB/s through the ring, no USB -- ran 12 × 60 s with an idle CPU
    and 15 × 60 s with `--work 1` (copy + ci8 pack of every block, as `serve`
    does): 0 overruns, no death. Everything the dying case does except USB
    is therefore fine.
  * **So it needs bulk IN near the bus limit.** The deaths happen only with
    ~28 MB/s leaving the device on the IN endpoint through the AIO path
    (`io_submit` is where the one captured panic was). TX -- 33.5 MB/s *into*
    the device on OUT, same AIO code -- has soaked for hours without one. Both
    kernel modules, so the suspects are FunctionFS's IN path, the ChipIdea
    UDC driver, or `sdrd`'s use of AIO on the IN endpoint, in a race that
    only bites at full rate; the corrupted-pointer panic in the submitting
    thread fits a use-after-free of a request or buffer.
  * Rate: roughly 1 death per 20–30 minutes at 16 MS/s ci8; none seen at
    ≤ 14 MS/s, but that was only tested for minutes.
  * `sdrd serve --fake-rx` (image: `mkimage --fake-rx`) feeds the RX USB
    path a synthetic block as fast as USB takes it, no DMA at all: 20 MS/s
    ci8, 40 MB/s, 0 overruns -- harder than real RX ever drives it. So the
    real-RX ceiling of 14 MS/s ci8 is on the ring side (uncached copy plus
    completion handling), not USB or conversion. It is the reproducer for
    the USB-side arm of this bug.
  * **And the USB side alone does not die either**: 25 × 60 s of fake RX at
    40 MB/s, clean. Every arm survives on its own; only the real combination
    -- ADC DMA into DDR at 64 MB/s, USB DMA out of it at ~28 MB/s, ring
    overrunning -- has ever died. Next discriminator: both at once in
    *separate processes* (fake RX + `rx-soak --work`), which share no
    driver state; a death there is the interconnect/DDR, survival puts it
    in the driver interplay.
  * **It died in separate processes too** (fake RX at 40 MB/s + `rx-soak
    --work` at 16 MS/s, ~7 min in), and this time the log caught an RCU
    stall: CPU0 hard-hung -- timer softirq frozen, RCU kthread never woken,
    an NMI from CPU1 unanswered. CPU0 takes both DMA completion IRQs and the
    USB IRQ. So: not sdrd, not one driver's logic; the shape is memory or
    bus corruption when the ADC DMA and the USB DMA both hit DDR flat out.
    The DDR fix only touched the *address* pins. Data and strobe pins were
    never swept.
  * `tools/tonecheck.py` verifies the fake-RX stream byte for byte on the
    host: any mismatch is a USB DMA read of DDR that came back wrong, which
    should show long before a hang does.
  * First verified combined run (`tools/combo-hammer.sh`, stock IOB
    settings): 28 GiB of fake-RX read back on the host with **0 wrong bytes**
    over 20 minutes, `rx-soak --work` overrunning underneath the whole time.
    Bit errors on the DMA read path are therefore not what precedes a hang,
    which weakens "DDR data pins" as the mechanism; a bus or interconnect
    lock-up (a CPU stuck on an access that never completes cannot answer an
    NMI, which matches the stall dump) fits at least as well. The slew
    comparison is still worth running: survival well past the baseline would
    be evidence either way.
  * That run died at 25.6 min with a third shape: "Bad mode in undefined
    instruction handler" -- an undefined instruction taken *inside* an
    exception handler, after which the Oops code itself faulted eight times
    on garbage task state. With the junk-PC panic and the NMI-deaf CPU0 hang,
    every death is the CPU executing or reading garbage; none has the shape
    of a driver logic bug. Stock-IOB baseline for the slew comparison: 25 min.
  * Not a missing erratum workaround: every Cortex-A9 r3p0 and PL310 r3p2
    erratum the kernel knows is enabled (764369 -- cache maintenance under
    DMA on MPCore -- 775420, 754322, 754327, 720789, 845369; PL310 588369,
    727915, 753970, 769419). The unset ones are for other cores.
  * **Slew 6/6 on the data and strobe pins (0x0004861c on SLCR 0xB60 and
    0xB64, applied at run time): 53 minutes, 24 passes, 77 GiB verified, no
    death** -- against a 25-minute stock baseline under the identical load.
    One run each, so a confirmation from a boot-time setting follows; if it
    holds, this is the second half of the same finding as the address pins:
    this layout wants slower edges than the FSBL gives it.
  * **Confirmed from a boot-time setting** (patch 0002 now takes all four
    pin groups from the device tree; `pluto.dts` sets data and strobe to
    0x0004861c): a second 25-pass run, 38 minutes, 77 GiB, no death. Two for
    two at slew 6/6 against two deaths at the default. This is the fix for
    "the CPU executes garbage under ADC DMA + USB DMA", by the same mechanism
    as the address-pin fix. The U-Boot `preboot` line has a matching
    extension ready in `build/flash/uboot-env-pmp2.bin` (`tools/mkenv.py`
    builds it; validated byte-for-byte against the flashed env).
  * `sdrd` now refuses a CONFIG above each format's measured cap
    (`max_rate_hz` in serve.rs, table in PROTOCOL.md), so a host cannot
    drive the device into the region where it dies.
  With the data/strobe slew fix in the device tree (and, once flashed, in
  U-Boot's `preboot`), the RX caps in `sdrd` are about dropped samples only,
  no longer about survival.
* The flash is driven in single-bit mode on purpose (no status-register
  writes, ever). Slower than quad reads; nothing here reads flash in bulk.
* Throughput, measured:
  * NCM/TCP (iperf3, IPv6 link-local): host→device 240 Mbit/s at ~29 % of one
    core, device→host 320 Mbit/s at ~72 %. Stock: 239 Mbit/s at 69 % of its
    only core.
  * pmpstream TX with queued AIO (16 × 16 KiB requests per endpoint,
    `crates/sdrd/src/aio.rs`): ceiling ≈ 33.5 MB/s, i.e. clean at 8 MS/s ci16,
    ~11.5 MS/s ci12, 16 MS/s ci8. With synchronous I/O it was 24.5 MB/s, which
    is what made full-duplex `tx ci16` loopback miss its tone now and then --
    TX starving into fill, not a bug in the stream. All format pairings now
    pass 99.8–100 % of sessions.
  * pmpstream RX: drop-free (`tools/droptest.sh`, 45–60 s, counted in
    hardware blocks after start-up) at 2.1 and 8 MS/s ci16, 11 MS/s ci12 and
    12 MS/s ci8. 14 MS/s ci8 looks clean over a minute but drops 0.66 % over
    an hour, so the cap is 12; 16 MS/s ci8 drops ~11 %.
    Before 2026-09-21 RX topped out near 6.5 MS/s in *every* format and nobody
    had noticed, because only TX had been rate-tested: see the uncached-ring
    lesson below. TX: drop-free at 2.1/8 (ci16), 11 (ci12) and 16 MS/s (ci8).
* `pmp loopback` at 4 MS/s full duplex shows a few RX overruns per run at
  ci16 (32 MB/s of USB in total); honest, and expected near the bus limit.
* busybox is a prebuilt static 1.31 binary; it does everything asked of it and
  stays (owner's decision).
* USB IDs are the pid.codes test pair 1209:0001.
* Sample rates below ~2.1 MS/s need the AD936x FIR decimators loaded.

## Flash from Linux

Mainline's `spi-zynq-qspi` refused 4-byte addresses, so spi-nor could not
probe a 32 MB part (`-22`). Patch 0004 lifts that and fixes the address phase
writing through a pointer into the caller's `spi_mem_op`. What it deliberately
does *not* allow is either stateful way of reaching beyond 16 MB -- "enter
4-byte mode" (B7h) and the extended address register (C5h) -- because both
survive a warm reset and the Zynq BootROM then cannot read its boot image
until a power cycle. This board's W25Q256 has the dedicated 4-byte opcodes, so
nothing is lost; a part without them fails to probe rather than endangering
the next boot. `SWP_KEEP` leaves the block-protection bits as found, the bus
is single-bit so the Quad Enable bit is never written, and the bootloader,
environment and firmware partitions are read-only in the device tree. Verified
by md5 against the backup, including above 16 MB.

Patch 0005 exposes the flash's 64-bit unique ID (`.../spi-nor/unique_id`).
`init` turns it into the 17-byte serial stock firmware reports -- an artefact of
ADI's driver (20 bytes clocked with no dummy phase, each XORed with its index,
bytes 3..19 kept), reproduced rather than understood, and checked against this
board -- so the USB serial number and the MAC addresses derived from it are the
same under either firmware.

## The Rust port of pmpstream

`os/kernel/pmpstream-rs` is what `make` builds and ships (owner's decision,
2026-09-21): `PMP_RUST=1 os/kernel/build.sh` produces a `CONFIG_RUST=y` kernel
in its own output directory, GCC-compiled, ~100 KB bigger. Same `/dev` nodes, same
ioctl ABI, same `.ko` name. What Rust-for-Linux 6.18 did and did not give it:

* Used as-is: platform driver + OF match, misc device with `mmap` and `ioctl`,
  `Mutex`, `CondVar`, `Arc`, user-memory access, firmware-node properties.
* Missing, so done through raw bindings inside small `unsafe` blocks:
  coherent DMA allocation *for another device* and its `mmap`, `ioremap`.
* Missing and not reachable by bindgen (static inlines): the whole dmaengine
  client API. `shim.c`, 60 lines, one C function per dmaengine call.
* No irqsave/bh `SpinLock`, and the completion callback is a tasklet. The ring
  counters are therefore atomics with one writer each, and queueing to the DMA
  sits behind a try-lock that never waits (the module's header comment has the
  argument). This was the part that most needed the soak to agree with it; it
  did (105 sessions, no drain timeouts, no restarts from idle, overrun counts
  identical to the C module's).

Two behaviours differ from the C on purpose: block counters are 64-bit (the
C's 32-bit `counter % nblocks` misorders the ring once, after 2³² blocks, when
`nblocks` is not a power of two), and a COMMIT interrupted by a signal returns
`EINTR` instead of restarting the ioctl, which in C commits the block twice.
Unbinding the driver with a node open is also safe now; in C it is a
use-after-free. One thing C gets for free had to be done by hand: the 6.18
misc-device abstraction leaves `file_operations.owner` unset, so the module
takes a reference on itself in `open` (checked: `rmmod` during a session is
refused, and works when idle).

## Lessons already paid for

* Leaving U-Boot's flash-DFU *continues* the boot; it does not reset. So
  `preboot` has already run with the old environment by then, and a new env
  takes effect from the next boot. Check the kernel's "was" values after a
  real reboot, not after the DFU exit.
* DMA-coherent memory is uncached, and code that reads it a sample at a time
  pays a DRAM access per load: the RX format converter ran at 6.5 MS/s
  straight off the ring and at full USB speed after one bulk copy of the block
  into ordinary memory (long bursts). Writes do not suffer the same way, which
  is why TX never showed it. Measure both directions.
* Reading a PL address with no AXI slave behind it hangs the bus -- no abort,
  no SIGBUS, the board just stops. The watchdog then resets it and it boots
  from flash by itself (seen 2026-09-21, the first unattended recovery on our
  own firmware). Do not probe the PL address space blindly.
* A source file restored from a tarball keeps its old mtime, and `make` then
  trusts the stale object built from the patched version. After *removing* a
  patch, `touch` what it touched -- and check a boot log for messages that
  should no longer exist.

* U-Boot's flash-DFU and RAM-DFU modes share a USB ID *and* an alt-setting
  name; `tools/ramboot.sh` refuses to upload unless it sees RAM-DFU's
  `dummy.dfu`.
* U-Boot enters DFU only if the Zynq `REBOOT_STATUS` register equals
  `0x0048000N` exactly. Reset-reason bits are sticky, so after a watchdog
  reset (bit 16) a write of just the code never matches. Write the whole
  register (`os/rootfs/usr/sbin/reboot-to`).
* chipidea needs the PHY as `phys = <...>`; with mainline Zynq boards'
  `usb-phy` it loses a probe-order race and silently never binds.
* `allnoconfig` drops things nobody thinks of as optional: `COMPAT_32BIT_TIME`
  (old static binaries see `sleep()` return instantly), every ARM erratum
  workaround, `VMAP_STACK`. `os/kernel/build.sh` reports requested options
  that did not survive; it cannot report the ones you never asked for.
* ChipIdea builds large requests from chained ~16 KiB transfer descriptors,
  and a short packet ends only the descriptor it lands in. Inside a multi-TD
  OUT request short packets delimit nothing and leave holes. Keep device-side
  OUT reads to one TD.
* Never interrupt synchronous FunctionFS I/O with a signal: the UDC can still
  complete the request afterwards, into an on-stack completion that is gone.
* A bulk pipe has no framing and requests outlive streams; whatever a dead
  host left half-transferred leads the next stream. Hence the start-of-stream
  marker in PROTOCOL.md. It only bites 3-byte samples, which is why ci8 and
  ci16 looked fine.
* Ring memory that userspace fills must be DMA-coherent on this CPU: the A9's
  VIPT D-cache lets a user alias and the kernel alias of one page sit in
  different cache sets, so the DMA API's flush misses what userspace wrote.
* For 1R1T on a CMOS interface the DAC core's rate register is 0, and the two
  unused converter channels must be parked explicitly or the DMA stream
  silently becomes I0,Q0,I1,Q1. When in doubt, dump the registers under stock
  firmware and diff (`tools/regdump.sh`).
* `/dev/kmsg` rate-limits and records each write() separately: log whole lines
  in one write.
* There is no serial console attached. `tools/lastlog.py` + `build/rescue.itb`
  read the previous boot's console out of RAM, and dev images return to DFU by
  themselves if no USB host appears. Nearly everything above was found that way.
