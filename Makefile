# pimpmypluto -- see docs/ARCHITECTURE.md
STOCK  := vendor/stock
# kernel compiler: gcc (default) or clang -- see docs/ARCHITECTURE.md before choosing clang
PMP_CC ?= gcc
# kernel sample-ring module: the Rust one (os/kernel/pmpstream-rs, on a
# CONFIG_RUST kernel) by default; `make PMP_RUST=` for the C original
PMP_RUST ?= 1
KOUT   := build/cs/kbuild$(if $(filter gcc,$(PMP_CC)),-gcc,)$(if $(PMP_RUST),-rust,)
MODDIR := build/out$(if $(PMP_RUST),-rust,)/modules
ZCC    := zig cc -target arm-linux-musleabihf -mcpu=cortex_a9 -Os -static -Wall -Wextra
USERLAND := build/out/mdnsd build/out/busybox build/out/dropbearmulti build/out/extra/sdrd
RUST_TARGET := armv7-unknown-linux-musleabihf
ifeq ($(shell uname),Darwin)
export LIBCLANG_PATH ?= /opt/homebrew/opt/llvm@21/lib
endif

all: build/pluto.itb

# Everything not in the repository: kernel source, busybox, dropbear (tools/fetch-deps.sh)
deps:
	tools/fetch-deps.sh
build/out/busybox build/out/dropbearmulti:
	tools/fetch-deps.sh busybox dropbear
build/cs/linux-6.18.52/Makefile:
	tools/fetch-deps.sh kernel

# The vendor bitstream (and, for rescue.itb, kernel) come off the board itself.
$(STOCK)/system.bit.bin $(STOCK)/zImage $(STOCK)/revb.dtb:
	@echo "$(STOCK) is empty: run tools/extract-stock.sh --board (or FILE) first; see README" >&2; exit 1

build/rescue.dtb: $(STOCK)/revb.dtb tools/mkrescue-dtb.sh
	tools/mkrescue-dtb.sh $@

build/out/mdnsd: os/src/mdnsd.c
	@mkdir -p build/out
	$(ZCC) -o $@ $<

# device daemon; cargo's own dependency tracking decides whether anything rebuilds
sdrd build/out/extra/sdrd: .FORCE
	cargo build --release --target $(RUST_TARGET) -p sdrd
	@mkdir -p build/out/extra
	cp target/$(RUST_TARGET)/release/sdrd build/out/extra/sdrd

host:
	cargo build --release -p pmp-cli

kernel: build/cs/linux-6.18.52/Makefile
	PMP_CC=$(PMP_CC) PMP_RUST=$(PMP_RUST) os/kernel/build.sh

# our userland + mainline kernel + stock bitstream
build/pluto.itb: kernel $(USERLAND) $(STOCK)/system.bit.bin $(shell find os/rootfs -type f) os/mkimage.py
	os/mkimage.py --kernel $(KOUT)/arch/arm/boot/zImage \
		--dtb $(KOUT)/arch/arm/boot/dts/xilinx/zynq-pmp-pluto.dtb \
		--moddir $(MODDIR) \
		--bitstream $(STOCK)/system.bit.bin --out $@

# our userland + known-good stock kernel; also reads the last boot's log
build/rescue.itb: $(USERLAND) $(STOCK)/zImage $(STOCK)/system.bit.bin $(shell find os/rootfs -type f) os/mkimage.py build/rescue.dtb
	os/mkimage.py --kernel $(STOCK)/zImage --dtb build/rescue.dtb \
		--bitstream $(STOCK)/system.bit.bin --out $@

boot: build/pluto.itb
	tools/ramboot.sh $<

rescue: build/rescue.itb
	tools/ramboot.sh $< && sleep 15 && tools/lastlog.py

.PHONY: all deps kernel sdrd host boot rescue .FORCE
.FORCE:
