#!/bin/sh
# Fetch and build everything `make` needs that is not in this repository:
#
#   build/cs/linux-$KVER/      mainline kernel source (kernel.org tarball, sha256-pinned)
#   build/out/busybox          busybox.net's prebuilt static armv7l binary (sha256-pinned)
#   build/out/dropbearmulti    dropbear, built here with zig cc, static
#
# plus the Rust cross target.  Idempotent: anything already present and
# matching is left alone.  The stock firmware (vendor/stock) is board-specific
# and comes from tools/extract-stock.sh instead.
#
#   tools/fetch-deps.sh [kernel|busybox|dropbear|rust]...     default: all
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/.." && pwd)
cd "$ROOT"

KVER=${KVER:-6.18.52}
KERNEL_URL="https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-$KVER.tar.xz"
KERNEL_SHA256=2b69564f7d4fea0c859b1959ba33709ee6e9139bd100e30a853b57159a8221b8

BUSYBOX_URL="https://busybox.net/downloads/binaries/1.31.0-defconfig-multiarch-musl/busybox-armv7l"
BUSYBOX_SHA256=cd04052b8b6885f75f50b2a280bfcbf849d8710c8e61d369c533acf307eda064

DROPBEAR_VER=2026.94
DROPBEAR_URL="https://matt.ucc.asn.au/dropbear/releases/dropbear-$DROPBEAR_VER.tar.bz2"
DROPBEAR_SHA256=e098034a843699200c8c977a991fff73159735bf795d5f72ef672c41a6b1ae81

DL=build/dl
mkdir -p "$DL" build/out

sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }

# fetch URL SHA256 -> build/dl/<basename>; re-downloads if the checksum is off
fetch() {
    f="$DL/$(basename "$1")"
    if [ -f "$f" ] && [ "$(sha256 "$f")" = "$2" ]; then echo "   have $f"; return; fi
    echo ">> $1"
    curl -fL --progress-bar -o "$f.part" "$1" && mv "$f.part" "$f"
    got=$(sha256 "$f")
    [ "$got" = "$2" ] || { echo "checksum mismatch for $f: $got" >&2; rm -f "$f"; exit 1; }
}

# The kernel tree needs a case-sensitive filesystem.  macOS's default is not,
# so build/cs is a sparse APFS image; on Linux it is a plain directory.
cs_mount() {
    [ "$(uname)" = Darwin ] || { mkdir -p build/cs; return; }
    if [ ! -d build/cs.sparsebundle ]; then
        echo ">> creating build/cs.sparsebundle (case-sensitive APFS, grows on demand)"
        hdiutil create -quiet -type SPARSEBUNDLE -fs 'Case-sensitive APFS' -size 16g \
            -volname pmp-cs build/cs.sparsebundle
    fi
    mkdir -p build/cs
    mount | grep -q " $ROOT/build/cs " ||
        hdiutil attach -quiet build/cs.sparsebundle -mountpoint "$ROOT/build/cs"
}

do_kernel() {
    cs_mount
    if [ -f "build/cs/linux-$KVER/Makefile" ]; then echo "   have build/cs/linux-$KVER"; return; fi
    fetch "$KERNEL_URL" "$KERNEL_SHA256"
    echo ">> unpacking linux-$KVER"
    tar -C build/cs -xJf "$DL/linux-$KVER.tar.xz"
}

do_busybox() {
    if [ -f build/out/busybox ] && [ "$(sha256 build/out/busybox)" = "$BUSYBOX_SHA256" ]; then
        echo "   have build/out/busybox"; return
    fi
    fetch "$BUSYBOX_URL" "$BUSYBOX_SHA256"
    cp "$DL/busybox-armv7l" build/out/busybox && chmod 755 build/out/busybox
}

do_dropbear() {
    stamp=build/out/dropbearmulti.version
    if [ -f build/out/dropbearmulti ] && [ "$(cat "$stamp" 2>/dev/null)" = "$DROPBEAR_VER" ]; then
        echo "   have build/out/dropbearmulti ($DROPBEAR_VER)"; return
    fi
    fetch "$DROPBEAR_URL" "$DROPBEAR_SHA256"
    src="build/dropbear-$DROPBEAR_VER"
    rm -rf "$src"; tar -C build -xjf "$DL/dropbear-$DROPBEAR_VER.tar.bz2"
    echo ">> building dropbear $DROPBEAR_VER with zig cc"
    (cd "$src" &&
        ./configure --host=arm-linux-musleabihf CC="$ROOT/tools/zcc" AR="$ROOT/tools/zar" \
            CFLAGS="-Os -fno-pie" LDFLAGS="-static -no-pie -s" --disable-zlib --disable-lastlog --disable-wtmp \
            --disable-utmp --disable-utmpx --disable-wtmpx --disable-pututline --disable-pututxline \
            >configure.log 2>&1 || { tail -20 configure.log; exit 1; }
        make -j"$(getconf _NPROCESSORS_ONLN)" PROGRAMS="dropbear dbclient dropbearkey scp" MULTI=1 \
            >make.log 2>&1 || { tail -30 make.log; exit 1; })
    cp "$src/dropbearmulti" build/out/dropbearmulti
    echo "$DROPBEAR_VER" > "$stamp"
    rm -rf "$src"
}

do_rust() {
    rustup target add armv7-unknown-linux-musleabihf
    rustup component add rust-src
}

[ $# -gt 0 ] || set -- rust busybox dropbear kernel
for what in "$@"; do "do_$what"; done
echo "ok"
