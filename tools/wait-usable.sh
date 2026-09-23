#!/bin/sh
# Block until the Pluto is usable again, then say how.  For when it is wedged
# and nobody can replug it: a watchdog reset, a crash, or a power blip will
# bring stock firmware back up with a working shell, and this notices.
#   tools/wait-usable.sh [max_hours]
# Exits 0 printing one line: DFU | OURS | STOCK-OK uptime=<s>.  Exits 1 on timeout.
cd "$(dirname "$0")/.." || exit 1
HOURS=${1:-9}; LOG=build/wait-usable.log; : > "$LOG"
ASK=$(mktemp "${TMPDIR:-/tmp}/askpass.XXXXXX"); printf '#!/bin/sh\necho analog\n' > "$ASK"; chmod 700 "$ASK"
trap 'rm -f "$ASK"' EXIT
end=$(( $(date +%s) + HOURS * 3600 )); last=
stock_shell() {   # a command that must *finish*; a wedged stock accepts the login and then hangs
    ( SSH_ASKPASS="$ASK" SSH_ASKPASS_REQUIRE=force ssh -o ConnectTimeout=5 -o UserKnownHostsFile=/dev/null \
        -o StrictHostKeyChecking=no -o LogLevel=ERROR -o PubkeyAuthentication=no root@192.168.2.1 \
        'cut -d. -f1 /proc/uptime' </dev/null 2>/dev/null & p=$!
      ( sleep 12; kill $p 2>/dev/null ) & k=$!; wait $p; rc=$?; kill $k 2>/dev/null; exit $rc )
}
while [ "$(date +%s)" -lt "$end" ]; do
    id=$(system_profiler SPUSBDataType 2>/dev/null | grep -i -o -E "Product ID: 0x(b673|b674|0001)" | head -1 | awk '{print $3}')
    [ "$id" = "$last" ] || { echo "$(date '+%F %T') usb: ${id:-absent}" >> "$LOG"; last=$id; }
    case "$id" in
        0xb674) echo "DFU"; exit 0 ;;
        0x0001) echo "OURS"; exit 0 ;;
        0xb673) up=$(stock_shell) && [ -n "$up" ] && { echo "STOCK-OK uptime=$up"; exit 0; } ;;
    esac
    sleep 45
done
echo "TIMEOUT after ${HOURS}h (last usb state: ${last:-absent})"; exit 1
