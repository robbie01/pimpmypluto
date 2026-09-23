#!/usr/bin/env python3
"""Verify a `sdrd serve --fake-rx` stream byte for byte.

The device sends one synthetic block over and over, so the stream is
periodic: 16 bytes in ci8 (measured, not derived -- the pattern below is what
the device actually emits).  Every byte that differs is a USB DMA read of DDR
that came back wrong: a corruption meter for the datapath, sensitive long
before anything crashes.

    pmp rx ... --format ci8 | tools/tonecheck.py
"""
import sys, time

PERIOD = bytes.fromhex("e800c33c00183c3c18003cc300e8c3c3")
P = len(PERIOD)
REPS = 16384
CHUNK = P * REPS
ref = PERIOD * REPS

def phase_of(chunk):
    """Which rotation of the period this chunk starts at, or None."""
    for ph in range(P):
        if chunk[:P * 4] == (PERIOD[ph:] + PERIOD[:ph]) * 4:
            return ph
    return None

total = bad = badchunks = resyncs = 0
t0 = last = time.time()
buf = bytearray()
inp = sys.stdin.buffer
while True:
    data = inp.read(CHUNK - len(buf))
    if not data:
        break
    buf += data
    if len(buf) < CHUNK:
        continue
    chunk = bytes(buf)
    buf = bytearray()
    total += CHUNK
    if chunk == ref:
        pass
    else:
        wrong = sum(1 for a, b in zip(chunk, ref) if a != b)
        i = next(k for k, (a, b) in enumerate(zip(chunk, ref)) if a != b)
        badchunks += 1
        bad += wrong
        print(f"{time.time()-t0:8.1f}s  CORRUPT: {wrong} bytes wrong in the chunk at byte {total-CHUNK}; "
              f"first at +{i}: got {chunk[i:i+16].hex()} want {ref[i:i+16].hex()}", flush=True)
        # A shifted stream (bytes lost) mismatches everywhere afterwards;
        # tell that apart from corruption by re-finding the phase.
        ph = phase_of(chunk[i - i % P:] if i >= P else chunk)
        if ph:
            resyncs += 1
            print(f"          stream phase shifted by {ph} bytes (bytes lost, not corrupted); resyncing", flush=True)
            ref = (PERIOD[ph:] + PERIOD[:ph]) * REPS
    now = time.time()
    if now - last >= 10:
        print(f"{now-t0:8.1f}s  {total/1e6/(now-t0):6.2f} MB/s  {total>>20} MiB checked, "
              f"{bad} bytes wrong in {badchunks} chunks, {resyncs} resyncs", flush=True)
        last = now
print(f"done: {total>>20} MiB, {bad} bytes wrong in {badchunks} chunks, {resyncs} resyncs", flush=True)
sys.exit(1 if bad else 0)
