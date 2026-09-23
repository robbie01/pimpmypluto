#!/bin/sh
# Build the mainline kernel + device tree for the Pluto.  Runs natively on
# macOS (LLVM from Homebrew, no VM, no container) and on Linux.
#
#   os/kernel/build.sh [make targets...]      default: zImage dtbs modules
#   PMP_CC=gcc|clang   compiler (default gcc; needs arm-none-eabi-gcc).  Each
#                      gets its own output directory, so both can coexist.
#   PMP_RUST=1         CONFIG_RUST kernel (os/kernel/rust.config on top) and the
#                      Rust pmpstream module; own output directory.  Needs
#                      rustc + rust-src and bindgen (see `make rustavailable`).
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
KVER=${KVER:-6.18.52}
SRC=${KSRC:-$ROOT/build/cs/linux-$KVER}
PMP_CC=${PMP_CC:-gcc}
case "$PMP_CC" in
    gcc)   TOOLCHAIN="CROSS_COMPILE=arm-none-eabi-"; OUT=${KOUT:-$ROOT/build/cs/kbuild-gcc} ;;
    clang) TOOLCHAIN="LLVM=1";                       OUT=${KOUT:-$ROOT/build/cs/kbuild} ;;
    *) echo "PMP_CC must be gcc or clang" >&2; exit 2 ;;
esac
JOBS=${JOBS:-$(getconf _NPROCESSORS_ONLN)}

HOSTCFLAGS=""
if [ "$(uname)" = Darwin ]; then
    # The source tree needs a case-sensitive filesystem: build/cs is a sparse
    # APFS image for exactly that reason.
    [ -d "$SRC" ] || hdiutil attach "$ROOT/build/cs.sparsebundle" -mountpoint "$ROOT/build/cs" >/dev/null
    PATH="/opt/homebrew/opt/llvm@21/bin:/opt/homebrew/opt/lld@21/bin:/opt/homebrew/opt/make/libexec/gnubin:/opt/homebrew/opt/gnu-sed/libexec/gnubin:/opt/homebrew/opt/coreutils/libexec/gnubin:/opt/homebrew/opt/findutils/libexec/gnubin:$PATH"
    export PATH
    export LIBCLANG_PATH="${LIBCLANG_PATH:-/opt/homebrew/opt/llvm@21/lib}"   # bindgen, PMP_RUST=1
    # elf.h/byteswap.h for the host tools; the -D's stop macOS's own uuid_t
    # colliding with the one in scripts/mod/file2alias.c.
    HOSTCFLAGS="-I$HERE/hostshim -include $HERE/hostshim/darwin-compat.h -D_UUID_T -D__GETHOSTUUID_H"
fi

# PMP_DEBUG=1: bug-hunting kernel (os/kernel/debug.config on top), own output dir.
CONFIG="$HERE/pluto.config"
if [ -n "${PMP_DEBUG:-}" ] || [ -n "${PMP_RUST:-}" ]; then
    OUT="$OUT${PMP_RUST:+-rust}${PMP_DEBUG:+-debug}"
    CONFIG="$OUT.config"; mkdir -p "$(dirname "$CONFIG")"
    cat "$HERE/pluto.config" ${PMP_RUST:+"$HERE/rust.config"} ${PMP_DEBUG:+"$HERE/debug.config"} > "$CONFIG.new"
    cmp -s "$CONFIG.new" "$CONFIG" 2>/dev/null && rm "$CONFIG.new" || mv "$CONFIG.new" "$CONFIG"
fi

mkdir -p "$OUT"
cp "$HERE/pluto.dts" "$SRC/arch/arm/boot/dts/xilinx/zynq-pmp-pluto.dts"
grep -q zynq-pmp-pluto "$SRC/arch/arm/boot/dts/xilinx/Makefile" ||
    sed -i.orig 's/^dtb-$(CONFIG_ARCH_ZYNQ) += \\$/&\n\tzynq-pmp-pluto.dtb \\/' "$SRC/arch/arm/boot/dts/xilinx/Makefile"

# Our few changes to the kernel proper.  Idempotent: a patch that reverses
# cleanly is already in.  (patch -N alone is not enough -- it happily re-applies
# a hunk that only inserts lines, once per build.)
for p in "$HERE"/patches/*.patch; do
    [ -z "${PMP_NOPATCH:-}" ] || break          # experimenting in the tree by hand
    if patch -d "$SRC" -p1 -R -s -f --dry-run < "$p" >/dev/null 2>&1; then continue; fi
    patch -d "$SRC" -p1 -s -f < "$p" || { echo "patch failed: $p" >&2; exit 1; }
done

mk() {
    make -C "$SRC" O="$OUT" ARCH=arm $TOOLCHAIN HOSTCFLAGS="$HOSTCFLAGS" -j"$JOBS" "$@"
}

if [ ! -f "$OUT/.config" ] || [ "$CONFIG" -nt "$OUT/.config" ]; then
    KCONFIG_ALLCONFIG="$CONFIG" mk allnoconfig
    # allnoconfig silently drops anything whose dependencies are unmet; say so.
    missing=$(grep -E '^CONFIG_[A-Z0-9_]+=y' "$CONFIG" | while read -r l; do
        grep -qxF "$l" "$OUT/.config" || echo "  $l"; done)
    [ -z "$missing" ] || { echo "requested but not enabled:"; echo "$missing"; }
fi

default=; [ $# -gt 0 ] || { default=1; set -- zImage dtbs modules; }
mk "$@"
[ -n "$default" ] || exit 0

# Out-of-tree modules, staged where os/mkimage.py picks them up.
MODDEST="$ROOT/build/out${PMP_RUST:+-rust}${PMP_DEBUG:+-debug}/modules"
mkdir -p "$MODDEST"
# pmpstream exists twice: the C original and its Rust port.  Same .ko name,
# same ABI; a PMP_RUST build ships the port.
for m in "$HERE/pmpstream${PMP_RUST:+-rs}" "$HERE"/a9reg; do
    stage="$OUT/extmod/$(basename "$m")"          # objects never mix between compilers
    rm -rf "$stage"; mkdir -p "$stage"
    for f in "$m"/*.[ch] "$m"/*.rs "$m"/Makefile; do [ ! -f "$f" ] || cp "$f" "$stage"/; done
    mk M="$stage" modules
    cp "$stage"/*.ko "$MODDEST/"
done
ls -l "$OUT/arch/arm/boot/zImage" "$OUT/arch/arm/boot/dts/xilinx/zynq-pmp-pluto.dtb" 2>/dev/null || true
