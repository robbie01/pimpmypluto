#!/bin/sh
# Force continuous RX overrun at a rate the board can otherwise sustain: the
# host reads slowly (throttled sink), so the device ring drops blocks all the
# time while nothing on the device is near its CPU ceiling.  Watches for a
# death exactly like tools/rxhammer.sh.
#   tools/overrun-hammer.sh [runs] [format] [rate] [seconds]
cd "$(dirname "$0")/.." || exit 1
runs=${1:-15}; fmt=${2:-ci8}; rate=${3:-8000000}; secs=${4:-60}
SSH="ssh -o UserKnownHostsFile=build/known_hosts -o ConnectTimeout=5 -o ServerAliveInterval=3 root@pluto.local"
boot0=$($SSH 'cat /proc/sys/kernel/random/boot_id') || { echo "board unreachable at start"; exit 2; }
for n in $(seq 1 "$runs"); do
    # throttle: read 64 KiB, pause, repeat -- roughly half the stream, so the
    # device is busy converting and submitting *and* its ring overruns.  The
    # loop ends at EOF; pmp's --seconds counts received samples, so it is
    # stopped by the clock instead.
    ./target/release/pmp rx --freq 915M --rate "$rate" --format "$fmt" 2>build/overrun.err |
        sh -c 'while [ "$(dd bs=65536 count=1 2>/dev/null | wc -c)" -gt 0 ]; do sleep 0.008; done' &
    p=$!; sleep "$secs"; pkill -f "pmp rx --freq 915M --rate $rate" 2>/dev/null; wait $p 2>/dev/null
    tail -1 build/overrun.err | sed "s/^/run $n: /"
    boot=$($SSH 'cat /proc/sys/kernel/random/boot_id' 2>/dev/null)
    if [ "$boot" != "$boot0" ]; then
        echo "run $n: BOARD DIED; waiting for it"
        for i in $(seq 1 60); do $SSH true 2>/dev/null && break; sleep 5; done
        out=build/lastlog-overrun-$(date +%H%M%S).txt
        $SSH 'mount -t pstore pstore /sys/fs/pstore 2>/dev/null; cat /sys/fs/pstore/console-ramoops-0' > "$out" 2>&1
        echo "dead boot's console saved to $out; its last lines:"; tail -5 "$out"
        exit 1
    fi
    sleep 3
done
echo "survived $runs runs of throttled rx $fmt $rate x ${secs}s"
