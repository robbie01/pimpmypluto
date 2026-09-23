#!/bin/sh
# Boot IMAGE, then run one pinned memory-stress process per core for SECONDS.
# The two-minute reproducer for "two busy cores lose cache lines".
cd "$(dirname "$0")/.." || exit 1
IMG=$1; SECS=${2:-180}
SSH="ssh -o BatchMode=yes -o ConnectTimeout=10 -o ServerAliveInterval=5 -o ServerAliveCountMax=6 -o LogLevel=ERROR -o UserKnownHostsFile=build/known_hosts -o StrictHostKeyChecking=accept-new root@pluto.local"
for i in $(seq 1 40); do system_profiler SPUSBDataType 2>/dev/null | grep -q -i -E "Product ID: 0x(b673|0001)" && break; sleep 3; done
system_profiler SPUSBDataType 2>/dev/null | grep -q -i "Product ID: 0xb673" && sleep 28
tools/ramboot.sh "$IMG" </dev/null 2>&1 | grep -E "refusing|never" && exit 3
ssh-keygen -R pluto.local -f build/known_hosts >/dev/null 2>&1; sleep 12
for i in $(seq 1 60); do dns-sd -t 1 -G v6 pluto.local 2>/dev/null | grep -q -i fe80 && break; done; sleep 5
$SSH 'echo "running $(uname -r), $(nproc) cpus; $(dmesg | grep -E "L2C-310 (full|enabling early|ID pref|dynamic)|L2C-310: CACHE" | sed "s/.*L2C-310[: ]*//" | tr "\n" "|")"' </dev/null || exit 3
$SSH "(taskset 1 memstress 1 96 $SECS 2>&1 | tail -1 | sed 's/^/cpu0: /') & (taskset 2 memstress 1 96 $SECS 2>&1 | tail -1 | sed 's/^/cpu1: /'); wait" </dev/null
