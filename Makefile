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
USERLAND := build/out/mdnsd build/out/busybox build/out/dropbearmulti

all: build/pluto.itb

build/out/mdnsd: os/src/mdnsd.c
	@mkdir -p build/out
	$(ZCC) -o $@ $<

kernel:
	PMP_CC=$(PMP_CC) PMP_RUST=$(PMP_RUST) os/kernel/build.sh

# our userland + mainline kernel + stock bitstream
build/pluto.itb: kernel $(USERLAND) $(shell find os/rootfs -type f) os/mkimage.py
	os/mkimage.py --kernel $(KOUT)/arch/arm/boot/zImage \
		--dtb $(KOUT)/arch/arm/boot/dts/xilinx/zynq-pmp-pluto.dtb \
		--moddir $(MODDIR) \
		--bitstream $(STOCK)/system.bit.bin --out $@

# our userland + known-good stock kernel; also reads the last boot's log
build/rescue.itb: $(USERLAND) $(shell find os/rootfs -type f) os/mkimage.py build/rescue.dtb
	os/mkimage.py --kernel $(STOCK)/zImage --dtb build/rescue.dtb \
		--bitstream $(STOCK)/system.bit.bin --out $@

boot: build/pluto.itb
	tools/ramboot.sh $<

rescue: build/rescue.itb
	tools/ramboot.sh $< && sleep 15 && tools/lastlog.py

.PHONY: all kernel boot rescue
