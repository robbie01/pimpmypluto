#!/bin/sh
# Write firmware (and optionally the U-Boot environment) to the Pluto's QSPI
# through U-Boot's *flash* DFU mode.  Never touches boot.dfu (FSBL + U-Boot):
# that region is the recovery path.  Everything this writes can be rewritten
# the same way; tools/restore-stock.sh puts the vendor's bytes back.
#
#   tools/flash.sh FIRMWARE.itb [ENV.bin]          dry run: says what it would do
#   tools/flash.sh --go FIRMWARE.itb [ENV.bin]     do it
#
# Get into flash DFU by holding the button while plugging in (or `reboot-to sf`).
set -eu
GO=; [ "${1:-}" = "--go" ] && { GO=1; shift; }
FW=${1:?firmware .itb}; ENV=${2:-}
ID=0456:b674
alts=$(dfu-util -l 2>/dev/null | grep "$ID" | sed 's/.*name="\([^"]*\)".*/\1/' | tr '\n' ' ')
case "$alts" in
    *boot.dfu*) ;;
    *dummy.dfu*) echo "board is in RAM-DFU mode; this tool needs flash DFU (hold the button while plugging in)" >&2; exit 1 ;;
    *) echo "no Pluto in DFU mode found (saw: ${alts:-nothing})" >&2; exit 1 ;;
esac
fwsize=$(stat -f %z "$FW" 2>/dev/null || stat -c %s "$FW")
[ "$fwsize" -le $((0x1E00000)) ] || { echo "firmware larger than the 30 MiB partition" >&2; exit 1; }
dumpimage -l "$FW" >/dev/null 2>&1 || { echo "$FW is not a FIT image" >&2; exit 1; }
[ -z "$ENV" ] || [ "$(stat -f %z "$ENV" 2>/dev/null || stat -c %s "$ENV")" -eq $((0x20000)) ] || { echo "environment image must be exactly 0x20000 bytes" >&2; exit 1; }
echo "flash DFU alts: $alts"
echo "plan: firmware.dfu <- $FW ($fwsize bytes)${ENV:+, then uboot-env.dfu <- $ENV}; boot.dfu untouched"
[ -n "$GO" ] || { echo "(dry run; add --go)"; exit 0; }

verify() {  # alt file: read the region back and compare the prefix we wrote
    tmp=$(mktemp "${TMPDIR:-/tmp}/dfu-readback.XXXXXX"); rm -f "$tmp"
    if dfu-util -d $ID -a "$1" -U "$tmp" >/dev/null 2>&1; then
        n=$(stat -f %z "$2" 2>/dev/null || stat -c %s "$2")
        if head -c "$n" "$tmp" | cmp -s - "$2"; then echo "   verified $1 ($n bytes read back identical)"; else echo "   READBACK OF $1 DIFFERS" >&2; rm -f "$tmp"; exit 1; fi
    else echo "   (this U-Boot does not support DFU upload for $1; not verified)"; fi
    rm -f "$tmp"
}
echo ">> writing firmware"; dfu-util -d $ID -a firmware.dfu -D "$FW" 2>&1 | grep -E "^(Download done|Error|dfu-util: [A-Z])" || true
verify firmware.dfu "$FW"
if [ -n "$ENV" ]; then
    # Environment LAST: on leaving DFU, U-Boot's script does `env save` of its own
    # in-memory copy if the last thing downloaded was the firmware.
    echo ">> writing U-Boot environment"; dfu-util -d $ID -a uboot-env.dfu -D "$ENV" 2>&1 | grep -E "^(Download done|Error|dfu-util: [A-Z])" || true
    verify uboot-env.dfu "$ENV"
fi
echo ">> leaving DFU; the board boots what was just written"
dfu-util -d $ID -a uboot-env.dfu -e >/dev/null 2>&1 || true
