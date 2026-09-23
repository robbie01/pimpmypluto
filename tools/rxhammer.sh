#!/bin/sh
# Repeat a high-rate RX session and notice if the board dies under it.  On a
# death: wait for it to come back (watchdog -> boots from flash), save the dead
# boot's console from pstore, stop.
#   tools/rxhammer.sh [runs] [format] [rate] [seconds]
cd "$(dirname "$0")/.." || exit 1
runs=${1:-15}; fmt=${2:-ci8}; rate=${3:-16000000}; secs=${4:-60}
SSH="ssh -o UserKnownHostsFile=build/known_hosts -o ConnectTimeout=5 -o ServerAliveInterval=3 root@pluto.local"
boot0=$($SSH 'cat /proc/sys/kernel/random/boot_id') || { echo "board unreachable at start"; exit 2; }
for n in $(seq 1 "$runs"); do
    tools/droptest.sh rx "$fmt" "$rate" "$secs" 2>&1 | grep -E "S/s:|heartbeats" | sed "s/^/run $n: /"
    pkill -f "pmp rx" 2>/dev/null
    boot=$($SSH 'cat /proc/sys/kernel/random/boot_id' 2>/dev/null)
    if [ "$boot" != "$boot0" ]; then
        echo "run $n: BOARD DIED (boot id changed or unreachable); waiting for it"
        for i in $(seq 1 60); do $SSH true 2>/dev/null && break; sleep 5; done
        out=build/lastlog-rxhammer-$(date +%H%M%S).txt
        $SSH 'mount -t pstore pstore /sys/fs/pstore 2>/dev/null; cat /sys/fs/pstore/console-ramoops-0' > "$out" 2>&1
        echo "dead boot's console saved to $out; its last lines:"; tail -5 "$out"
        exit 1
    fi
    sleep 3
done
echo "survived $runs runs of rx $fmt $rate x ${secs}s"
