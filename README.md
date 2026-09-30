# pimpmypluto

Replacement firmware for a PlutoSDR-compatible board: mainline Linux, no IIO,
a small documented USB protocol, sample formats beyond 16-bit I/Q, and a
datapath that cannot starve the DAC.

Start with [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — including its
DDR section: this unit needed a hardware-level fix before anything was stable.
[docs/PROTOCOL.md](docs/PROTOCOL.md) is the USB interface;
[docs/RESTORE.md](docs/RESTORE.md) is how to get back to stock (so far:
unplug it — nothing here has written the flash).

## Building

Prerequisites (the versions this was last built with; newer usually works):

| tool | version | for |
|---|---|---|
| Rust via rustup | 1.98.1 (pinned in `rust-toolchain.toml`, pulls the armv7 musl target and `rust-src`) | crates, kernel Rust module |
| zig | 0.16 | C cross compiler/linker for everything on the device (`tools/zcc`) |
| bindgen (`cargo install bindgen-cli`) | 0.73 | `sdrd`'s no-OS bindings, kernel Rust |
| LLVM/clang + lld | 21 | libclang for bindgen; `PMP_CC=clang` kernel builds |
| arm-none-eabi-gcc | 16 | the default kernel compiler |
| GNU make, sed, coreutils, findutils | make 4.4 | kernel build on macOS (Homebrew's `gnubin`) |
| u-boot-tools (`mkimage`, `dumpimage`), dtc | 2026.07, 1.8 | FIT images, device trees |
| dfu-util | 0.11 | booting and flashing the board |
| python3 | 3.x | image builder, tools |

macOS: `brew install zig llvm@21 lld@21 arm-none-eabi-gcc make gnu-sed coreutils findutils u-boot-tools dtc dfu-util`.
The kernel tree needs a case-sensitive filesystem; `tools/fetch-deps.sh` makes
a sparse APFS image at `build/cs.sparsebundle` for it.  Linux: the same tools
from your distribution.

Two things are not in the repository. `tools/fetch-deps.sh` downloads and
checksums the kernel source (6.18.52) and busybox (busybox.net's static 1.31
binary) and builds dropbear from source with zig. The vendor's FPGA bitstream
is a board-specific build that exists nowhere else, so it is taken from the
board: `tools/extract-stock.sh --board` makes a full flash backup over ssh
(stock firmware, password `analog`) and unpacks it into `vendor/stock`.

```sh
make deps                              # kernel source, busybox, dropbear, rust target
tools/extract-stock.sh --board         # once, from the board running stock firmware
make                                   # kernel + modules + sdrd + image   (macOS or Linux, no VM)
make host                              # host tool: target/release/pmp

tools/ramboot.sh build/pluto.itb       # boot it from RAM over USB DFU
target/release/pmp info
target/release/pmp tx --freq 915M --rate 4M --format ci8 --tone 100k
examples/plutotv.sh --pattern          # NTSC colour bars via ~/plutotv's encoder

# after a crash (the board falls back to stock firmware by itself):
make build/rescue.itb && tools/ramboot.sh build/rescue.itb && tools/lastlog.py
```

Prebuilt parts of a release (`pimpmypluto-device-*.tar.gz`: kernel, device
tree, modules, sdrd, busybox, dropbear, mdnsd) skip everything but the last
step: unpack it as `build/out`, then
`os/mkimage.py --kernel build/out/zImage --dtb build/out/zynq-pmp-pluto.dtb --bitstream vendor/stock/system.bit.bin --out build/pluto.itb`.
No prebuilt `.itb` is published: each one carries the builder's ssh keys.

Images are reachable by ssh with your `~/.ssh/id_*.pub` (or
`build/secrets/authorized_keys`) and carry a host key generated once into
`build/secrets/`, so two builds are never byte-identical by design.

| | |
|---|---|
| `os/` | kernel config, device tree, `pmpstream` DMA-ring module (Rust, `os/kernel/pmpstream-rs`; the C original beside it builds with `make PMP_RUST=`), initramfs, image builder |
| `crates/sdrd` | device daemon (Rust): radio via ADI's no-OS driver, USB function, formats, health policy |
| `crates/pmp-proto`, `pmp-convert` | wire protocol and sample formats, shared by both ends, tested on the host |
| `crates/pmpstream`, `pmp-cli` | host library (nusb) and the `pmp` command |
| `third_party/no-OS` | the subset of ADI's no-OS we link, unmodified |
| `tools/` | dependency fetcher, stock firmware extractor, RAM boot, flash, post-mortem log reader, soak test, two-minute memory test (`memtest.sh`), DDR pin sweep, register dump |

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
