#!/bin/sh
# Sweep the DDR *address-pin* drive/slew setting (SLCR DDRIOB_DRIVE_SLEW_ADDR,
# 0xF8000B5C) on the live board and count lost DDR writes for each value.
# Background: docs/ARCHITECTURE.md, "DDR address bus".  Uses the two-core
# kernel image because it fails fastest (~10+ lost bursts per 100 s at the
# vendor's setting); a bad value simply crashes the board back to stock.
#
#   tools/ddr-sweep.sh [IMAGE] [SECONDS_PER_TRIAL]
cd "$(dirname "$0")/.." || exit 1
IMG=${1:-build/pluto-845369.itb}; SECS=${2:-100}
SSH="ssh -o BatchMode=yes -o ConnectTimeout=10 -o ServerAliveInterval=5 -o ServerAliveCountMax=6 -o LogLevel=ERROR -o UserKnownHostsFile=build/known_hosts -o StrictHostKeyChecking=accept-new root@pluto.local"

boot() {
    for i in $(seq 1 40); do p=$(system_profiler SPUSBDataType 2>/dev/null | grep -i -o -E "Product ID: 0x(b673|b674|0001)" | head -1); [ -n "$p" ] && break; sleep 3; done
    [ -n "$p" ] || { echo "board is not on USB: replug it" >&2; exit 3; }
    echo "$p" | grep -q b673 && sleep 28
    tools/ramboot.sh "$IMG" </dev/null 2>&1 | grep -E "refusing|never" && exit 3
    ssh-keygen -R pluto.local -f build/known_hosts >/dev/null 2>&1; sleep 12
    for i in $(seq 1 60); do dns-sd -t 1 -G v6 pluto.local 2>/dev/null | grep -q -i fe80 && break; done; sleep 5
    $SSH 'echo "# $(uname -r), $(nproc) cpus, vendor DRIVE_SLEW_ADDR=$(devmem 0xF8000B5C 32)"' </dev/null
}

# value = DRIVE_P[6:0] | DRIVE_N[13:7] | SLEW_P[18:14] | SLEW_N[23:19]
val() { printf '0x%08X' $(( $1 | ($2 << 7) | ($3 << 14) | ($4 << 19) )); }

boot
#            drive P  N   slew P  N
for t in    "0x1C 0x0C  3  3  vendor/Xilinx default" \
            "0x1C 0x0C  0  0  slowest edges" \
            "0x1C 0x0C  1  1  " \
            "0x1C 0x0C  6  6  " \
            "0x1C 0x0C 31 31  fastest (as data pins)" \
            "0x30 0x18  3  3  stronger drive" \
            "0x10 0x08  3  3  weaker drive"; do
    set -- $t; v=$(val $1 $2 $3 $4); shift 4
    out=$($SSH "devmem 0xF8000008 32 0xDF0D; devmem 0xF8000B5C 32 $v; (taskset 1 memstress 1 96 $SECS 2>&1 | tail -1) & (taskset 2 memstress 1 96 $SECS 2>&1 | tail -1); wait" </dev/null 2>&1) ||
        { echo "$v  $*: BOARD DIED -- rebooting it"; boot; continue; }
    echo "$out" | awk -v v="$v" -v l="$*" '/errors/ {e+=$(NF-1); p+=$(NF-3)} END {printf "%s  %-26s passes %3d  LOST-WRITE ERRORS %d\n", v, l, p, e}'
done
