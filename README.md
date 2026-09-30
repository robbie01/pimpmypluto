# pimpmypluto

Replacement firmware for a PlutoSDR-compatible board: mainline Linux, no IIO,
a small documented USB protocol, sample formats beyond 16-bit I/Q, and a
datapath that cannot starve the DAC.

Start with [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — including its
DDR section: this unit needed a hardware-level fix before anything was stable.
[docs/PROTOCOL.md](docs/PROTOCOL.md) is the USB interface;
[docs/RESTORE.md](docs/RESTORE.md) is how to get back to stock (so far:
unplug it — nothing here has written the flash).

```sh
make                                   # kernel + module + image   (macOS or Linux, no VM)
export LIBCLANG_PATH=/opt/homebrew/opt/llvm@21/lib
cargo build --release --target armv7-unknown-linux-musleabihf -p sdrd \
    && cp target/armv7-unknown-linux-musleabihf/release/sdrd build/out/extra/
cargo build --release -p pmp-cli       # host tool: target/release/pmp

tools/ramboot.sh build/pluto.itb       # boot it from RAM over USB DFU
target/release/pmp info
target/release/pmp tx --freq 915M --rate 4M --format ci8 --tone 100k
examples/plutotv.sh --pattern          # NTSC colour bars via ~/plutotv's encoder

# after a crash (the board falls back to stock firmware by itself):
tools/ramboot.sh build/rescue.itb && tools/lastlog.py
```

| | |
|---|---|
| `os/` | kernel config, device tree, `pmpstream` DMA-ring module (Rust, `os/kernel/pmpstream-rs`; the C original beside it builds with `make PMP_RUST=`), initramfs, image builder |
| `crates/sdrd` | device daemon (Rust): radio via ADI's no-OS driver, USB function, formats, health policy |
| `crates/pmp-proto`, `pmp-convert` | wire protocol and sample formats, shared by both ends, tested on the host |
| `crates/pmpstream`, `pmp-cli` | host library (nusb) and the `pmp` command |
| `third_party/no-OS` | the subset of ADI's no-OS we link, unmodified |
| `tools/` | RAM boot, post-mortem log reader, soak test, two-minute memory test (`memtest.sh`), DDR pin sweep, register dump |

## License

pimpmypluto is free software under the GNU Affero General Public License,
version 3 or later — see [LICENSE](LICENSE).

Parts that must carry other terms keep them:

| | |
|---|---|
| `os/kernel/pmpstream`, `os/kernel/pmpstream-rs`, `os/kernel/a9reg`, `os/kernel/patches` | Linux kernel modules and patches: GPL-2.0 (`MODULE_LICENSE("GPL")`) |
| `os/kernel/hostshim/elf.h` | musl's `elf.h`, MIT |
| `third_party/no-OS` | ADI's no-OS driver, unmodified, under its own licences (see the `LICENSE*` files there) |
| `vendor/tezuka-nano` | reference copies from F5OEO's tezuka_fw / maia-sdr, under their licences |
