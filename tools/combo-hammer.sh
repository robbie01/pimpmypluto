#!/bin/sh
# Two DMA masters on DDR from two processes, until something gives: the
# device streams fake RX (host verifies every byte, tools/tonecheck.py) while
# it also runs `sdrd rx-soak --work` at 16 MS/s.  Stops on a death and saves
# the dead boot's console.  Needs the --fake-rx image.
#   tools/combo-hammer.sh [passes] [seconds per pass] [label]
cd "$(dirname "$0")/.." || exit 1
passes=${1:-15}; secs=${2:-120}; label=${3:-combo}
SSH="ssh -o UserKnownHostsFile=build/known_hosts -o ConnectTimeout=5 -o ServerAliveInterval=3 root@pluto.local"
boot0=$($SSH 'cat /proc/sys/kernel/random/boot_id') || { echo "board unreachable"; exit 2; }
$SSH 'dmesg | grep -q "FAKE RX"' || { echo "device is not serving fake RX (boot build/pluto-fakerx.itb)"; exit 2; }
$SSH "(sdrd rx-soak --rate 16000000 --sessions $((passes * secs / 60 + 3)) --seconds 60 --work 1 > /tmp/rxsoak.log 2>&1) </dev/null >/dev/null 2>&1 &"
t0=$(date +%s)
for n in $(seq 1 "$passes"); do
    ./target/release/pmp rx --freq 915M --rate 14000000 --format ci8 --seconds "$secs" 2>"build/$label.err" |
        tools/tonecheck.py > "build/$label-pass$n.log" 2>&1
    printf 'pass %d (%ds): %s\n' "$n" $(( $(date +%s) - t0 )) "$(tail -1 "build/$label-pass$n.log")"
    grep CORRUPT "build/$label-pass$n.log" | head -3
    boot=$($SSH 'cat /proc/sys/kernel/random/boot_id' 2>/dev/null)
    if [ "$boot" != "$boot0" ]; then
        echo "pass $n: BOARD DIED after $(( $(date +%s) - t0 )) s; waiting for it"
        for i in $(seq 1 60); do $SSH true 2>/dev/null && break; sleep 5; done
        out=build/lastlog-$label-$(date +%H%M%S).txt
        $SSH 'mount -t pstore pstore /sys/fs/pstore 2>/dev/null; cat /sys/fs/pstore/console-ramoops-0' > "$out" 2>&1
        echo "dead boot's console saved to $out"; grep -m3 -E "stall|Oops|Unable|panic" "$out"
        exit 1
    fi
    sleep 2
done
$SSH 'pkill -f rx-soak' 2>/dev/null
echo "survived $passes passes ($(( $(date +%s) - t0 )) s)"
