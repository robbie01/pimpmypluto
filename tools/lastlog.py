#!/usr/bin/env python3
"""Print the kernel console log left in RAM by the previous boot.

Our kernel logs to a ramoops region (see os/kernel/pluto.dts).  DDR contents
survive a warm reset on the Zynq, so after a crash, a watchdog reset or the
dev failsafe, boot build/rescue.itb -- stock kernel, same region fenced off --
and run this to see what the dead kernel had to say:

    tools/ramboot.sh build/rescue.itb && tools/lastlog.py
    tools/lastlog.py --file dump.bin        # decode a saved dump
"""
import argparse, struct, subprocess, sys
from pathlib import Path

BASE, SIZE = 0x1BF00000, 0x100000
ROOT = Path(__file__).resolve().parent.parent
SIG = b"DBGC"                       # persistent_ram_buffer signature


def fetch(host):
    cmd = ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=8", "-o", "LogLevel=ERROR",
           "-o", "UserKnownHostsFile=%s" % (ROOT / "build" / "known_hosts"),
           "-o", "StrictHostKeyChecking=accept-new", "root@" + host,
           "memdump %#x %#x" % (BASE, SIZE)]
    return subprocess.run(cmd, check=True, capture_output=True).stdout


def zones(mem):
    """Each ramoops zone starts with {sig, start, size} then a ring of `size`
    valid bytes whose write position is `start`."""
    at = mem.find(SIG)
    while at >= 0:
        start, size = struct.unpack_from("<II", mem, at + 4)
        nxt = mem.find(SIG, at + 12)
        cap = (nxt if nxt >= 0 else len(mem)) - (at + 12)
        if 0 < size <= cap and start <= size:
            data = mem[at + 12:at + 12 + size]
            yield at, data[start:] + data[:start] if size == cap else data
        at = nxt


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--host", default="pluto.local")
    ap.add_argument("--file", help="decode a saved dump instead of asking the device")
    ap.add_argument("--save", help="also write the raw region here")
    a = ap.parse_args()

    mem = Path(a.file).read_bytes() if a.file else fetch(a.host)
    if a.save:
        Path(a.save).write_bytes(mem)
    found = False
    for off, data in zones(mem):
        found = True
        print("==== zone at +0x%x, %d bytes" % (off, len(data)))
        sys.stdout.write(data.decode("utf-8", "replace"))
        print()
    if not found:
        sys.exit("no log found: the kernel died before pstore came up, or RAM did not survive the reset")


if __name__ == "__main__":
    main()
