#!/bin/sh
# Populate vendor/stock from the board's own firmware.  Every image this
# project builds still carries the vendor's FPGA bitstream, and the rescue
# image carries the vendor's kernel; both come from the FIT the board shipped
# with, which is a vendor build that exists nowhere else.  Nothing here is
# committed: vendor/stock is gitignored.
#
#   tools/extract-stock.sh FILE          FILE is a FIT (.itb/.frm) or a raw dump
#                                        of the firmware partition (mtd3)
#   tools/extract-stock.sh --board       take a full flash backup from the board
#                                        over ssh (stock firmware, password
#                                        "analog") into $BACKUP_DIR and extract
#                                        from that.  This is the backup
#                                        docs/RESTORE.md restores from.
#
# Output (vendor/stock/):  stock.itb, system.bit.bin, zImage, revb.dtb, revb.dts,
# rootfs.cpio.gz, rootfs/ (unpacked).  Each part is checked against the MD5 the
# FIT itself carries.
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/.." && pwd)
OUT="$ROOT/vendor/stock"
ADDR=${PLUTO_ADDR:-192.168.2.1}
SSHO="-o ConnectTimeout=5 -o UserKnownHostsFile=/dev/null -o StrictHostKeyChecking=no -o LogLevel=ERROR -o PubkeyAuthentication=no"
for t in dumpimage dtc python3; do command -v $t >/dev/null || { echo "need $t" >&2; exit 1; }; done

backup_board() {
    B=${BACKUP_DIR:-$HOME/pluto-backup-$(date +%Y%m%d)}
    mkdir -p "$B"
    ASKPASS=$(mktemp "${TMPDIR:-/tmp}/askpass.XXXXXX"); printf '#!/bin/sh\necho analog\n' > "$ASKPASS"; chmod 700 "$ASKPASS"
    trap 'rm -f "$ASKPASS"' EXIT
    # shellcheck disable=SC2086
    dossh() { SSH_ASKPASS="$ASKPASS" SSH_ASKPASS_REQUIRE=force ssh $SSHO "root@$ADDR" "$@"; }
    echo ">> $ADDR: reading the whole 32 MB flash into $B (about a minute)"
    dossh 'cat /proc/mtd; for i in 0 1 2 3; do md5sum /dev/mtd${i}ro; done; fw_printenv 2>/dev/null' > "$B/device-side.txt"
    for i in 0 1 2 3; do
        dossh "cat /dev/mtd${i}ro" > "$B/mtd$i.bin"
        want=$(grep "/dev/mtd${i}ro" "$B/device-side.txt" | cut -d' ' -f1)
        got=$(md5 -q "$B/mtd$i.bin" 2>/dev/null || md5sum "$B/mtd$i.bin" | cut -d' ' -f1)
        [ "$want" = "$got" ] || { echo "mtd$i: device says $want, we read $got" >&2; exit 1; }
        echo "   mtd$i.bin $(wc -c < "$B/mtd$i.bin" | tr -d ' ') bytes, md5 verified"
    done
    echo "   keep $B somewhere safe: it is the only copy of this board's firmware"
    SRC="$B/mtd3.bin"
}

case "${1:-}" in
    --board) backup_board ;;
    "") sed -n '2,20p' "$0"; exit 2 ;;
    *) SRC=$1 ;;
esac

mkdir -p "$OUT"
# A partition dump is the FIT followed by erased flash; the FIT header says
# how long the FIT is.
python3 - "$SRC" "$OUT/stock.itb" <<'EOF'
import struct, sys
src, dst = sys.argv[1:]
with open(src, "rb") as f:
    hdr = f.read(8)
    magic, total = struct.unpack(">II", hdr)
    if magic != 0xD00DFEED:
        sys.exit(f"{src}: not a FIT/FDT image (magic {magic:#x})")
    f.seek(0)
    data = f.read(total)
if len(data) != total:
    sys.exit(f"{src}: truncated ({len(data)} of {total} bytes)")
open(dst, "wb").write(data)
print(f"   stock.itb: {total} bytes")
EOF

listing=$(dumpimage -l "$OUT/stock.itb" 2>/dev/null)
# index of the image whose node is named $1 (fpga@1, linux_kernel@1, ...)
idx() { printf '%s\n' "$listing" | sed -n "s/^ Image \([0-9]*\) ($1)\$/\1/p"; }
want_md5() { printf '%s\n' "$listing" | awk -v n=" Image $1 (" 'index($0,n)==1{f=1;next} /^ Image /{f=0} f&&/Hash value:/{print $3;exit}'; }
extract() {  # node file
    i=$(idx "$1"); [ -n "$i" ] || { echo "no image $1 in FIT" >&2; exit 1; }
    dumpimage -T flat_dt -p "$i" -o "$OUT/$2" "$OUT/stock.itb" >/dev/null 2>&1
    want=$(want_md5 "$i")
    if [ -n "$want" ]; then
        got=$(md5 -q "$OUT/$2" 2>/dev/null || md5sum "$OUT/$2" | cut -d' ' -f1)
        [ "$want" = "$got" ] || { echo "$2: FIT says md5 $want, extracted $got" >&2; exit 1; }
        echo "   $2 <- $1, md5 $got verified"
    else
        echo "   $2 <- $1"
    fi
}
extract fpga@1 system.bit.bin
extract linux_kernel@1 zImage
extract ramdisk@1 rootfs.cpio.gz
# The FIT names one device tree per board revision; config@1.. use fdt@2 (RevB), which
# is what this board's U-Boot picks.
extract fdt@2 revb.dtb
dtc -q -I dtb -O dts -o "$OUT/revb.dts" "$OUT/revb.dtb"
rm -rf "$OUT/rootfs"; mkdir "$OUT/rootfs"
(cd "$OUT/rootfs" && gzip -dc ../rootfs.cpio.gz | cpio -idm --quiet 2>/dev/null) || echo "   (rootfs unpack incomplete; not needed for the build)"
echo "ok: vendor/stock is populated"
