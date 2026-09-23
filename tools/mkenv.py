#!/usr/bin/env python3
"""Build a U-Boot environment image the way this board's U-Boot lays it out:
4-byte CRC32, no redundancy flag, NUL-separated vars, 0xff padding to 128 KiB.
Validated by reproducing build/flash/uboot-env-pmp.bin byte for byte.
    tools/mkenv.py IN.txt OUT.bin       (one VAR=value per line, order kept)
"""
import sys, zlib, struct
SIZE = 0x20000
vars = [l for l in open(sys.argv[1]).read().splitlines() if l and not l.startswith('#')]
body = (b''.join(v.encode() + b'\0' for v in vars) + b'\0').ljust(SIZE - 4, b'\xff')
open(sys.argv[2], 'wb').write(struct.pack('<I', zlib.crc32(body) & 0xffffffff) + body)
