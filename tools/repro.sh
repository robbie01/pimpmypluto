#!/bin/sh
# Reproducer for the USB-IN kernel crash: boot IMAGE from RAM, check what is
# actually running, then push NCM traffic device->host until it dies or
# SECONDS pass.   tools/repro.sh IMAGE [SECONDS]      exit 0 = survived
cd "$(dirname "$0")/.." || exit 1
IMG=${1:?image}; SECS=${2:-300}
KH=build/known_hosts
SSH="ssh -o BatchMode=yes -o ConnectTimeout=8 -o LogLevel=ERROR -o UserKnownHostsFile=$KH -o StrictHostKeyChecking=accept-new root@pluto.local"

# wait for whatever is there now (stock after a crash, or ours), then boot IMG
for i in $(seq 1 40); do
    system_profiler SPUSBDataType 2>/dev/null | grep -q -i -E "Product ID: 0x(b673|b674|0001)" && break; sleep 3
done
if system_profiler SPUSBDataType 2>/dev/null | grep -q -i "Product ID: 0xb673"; then sleep 28; fi   # stock: let sshd come up
tools/ramboot.sh "$IMG" </dev/null 2>&1 | grep -E "refusing|never" && exit 3
ssh-keygen -R pluto.local -f $KH >/dev/null 2>&1
sleep 12
for i in $(seq 1 60); do dns-sd -t 1 -G v6 pluto.local 2>/dev/null | grep -q -i fe80 && break; done
sleep 5
$SSH 'echo "running: $(dmesg | sed -n "s/.*Linux version \([^ ]*\) .*(\(.*\)) #.*/\1 \2/p" | head -1), $(nproc) cpu, ffs=$(ls /sys/kernel/config/usb_gadget/pmp/configs/c.1/ | grep -c ffs), sdrd=$(pidof sdrd >/dev/null && echo up || echo no)"
      setsid iperf3 -s </dev/null >/dev/null 2>&1 &
      sleep 1; true' </dev/null || exit 3
LL=$(dns-sd -t 1 -G v6 pluto.local 2>/dev/null | grep -i -o -E "fe80[0-9a-f:]+%en[0-9]+" | head -1)
start=$(date +%s)
iperf3 -6 -c "$LL" -t "$SECS" -R -i 0 </dev/null >/dev/null 2>&1
took=$(( $(date +%s) - start ))
if $SSH true </dev/null 2>/dev/null; then echo "SURVIVED ${took}s"; exit 0; fi
echo "DIED within ${took}s (includes iperf's ~60 s idle timeout)"; exit 1
