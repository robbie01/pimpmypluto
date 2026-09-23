#!/bin/sh
# Steady-state drop test: stream one direction for a while at a given format and
# rate and report how many hardware blocks were missed *after* start-up, from
# sdrd's own 10-second heartbeat ("alive tx blocks/underruns rx blocks/overruns").
#   tools/droptest.sh tx|rx FORMAT RATE [SECONDS]      (needs target/release/pmp)
cd "$(dirname "$0")/.." || exit 1
dir=$1; fmt=$2; rate=$3; secs=${4:-45}
SSH="ssh -o UserKnownHostsFile=build/known_hosts -o ConnectTimeout=5 -o ServerAliveInterval=3 root@pluto.local"
mark="droptest-$$-$(date +%s)"
$SSH "echo 'pmp: $mark begin' > /dev/kmsg" || exit 2
if [ "$dir" = tx ]; then
    ./target/release/pmp tx --freq 915M --rate "$rate" --format "$fmt" --tone 100k --atten 40 >/dev/null 2>build/droptest.err &
else
    ./target/release/pmp rx --freq 915M --rate "$rate" --format "$fmt" --seconds $((secs + 5)) 2>build/droptest.err | wc -c > build/droptest.bytes &
fi
pid=$!
perl -e "select(undef,undef,undef,$secs)"
$SSH "echo 'pmp: $mark end' > /dev/kmsg"
[ "$dir" = tx ] && kill $pid 2>/dev/null
wait $pid 2>/dev/null
col=$([ "$dir" = tx ] && echo 1 || echo 3)
$SSH dmesg | sed -n "/$mark begin/,/$mark end/p" | grep "sdrd: alive" |
    sed -E 's|.*alive +tx ([0-9]+)/([0-9]+)[^0-9]+rx ([0-9]+)/([0-9]+).*|\1 \2 \3 \4|' | awk -v c=$col -v d="$dir" -v f="$fmt" -v r="$rate" '
    { n++; if (n == 2) { b0 = $c; m0 = $(c+1) } b1 = $c; m1 = $(c+1) }
    END { if (n < 3) { print d, f, r, ": too few heartbeats (" n ")"; exit 1 }
          printf "%s %-4s %9s S/s: %d blocks, %d missed in steady state (%d heartbeats; %d missed during start-up)\n", d, f, r, b1 - b0, m1 - m0, n, m0 }'
[ -s build/droptest.err ] && head -2 build/droptest.err
exit 0
