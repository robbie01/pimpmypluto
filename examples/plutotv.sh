#!/bin/sh
# NTSC television on the new firmware: plutotv's encoder piped straight into
# pmpstream.  No responder to install, no ssh, no supervisor: the firmware
# speaks int8 natively, never starves the DAC, mutes itself if this pipeline
# dies and picks up again when it returns.
#
#   examples/plutotv.sh [--channel N] [ntsctx args...]        e.g.
#   examples/plutotv.sh --pattern
#   examples/plutotv.sh ~/Downloads/badapple.mp4 --loop
#
# Signal constants are plutotv's (see its README): everything is a multiple of
# the line rate fH = 2*fsc/455.
set -eu
PLUTOTV=${PLUTOTV:-$HOME/plutotv}
PMP=${PMP:-$(dirname "$0")/../target/release/pmp}
ATTEN=${ATTEN:-30}          # dB below full power; 0 is what plutotv's Makefile used, on a cable

channel=14
if [ "${1:-}" = "--channel" ]; then channel=$2; shift 2; fi

# US broadcast picture carriers: ch 2-4 55.25+6n, 5-6 77.25+, 7-13 175.25+, 14+ 471.25+6n
picture=$(awk -v c="$channel" 'BEGIN {
    if (c >= 14) f = 471.25 + 6 * (c - 14); else if (c >= 7) f = 175.25 + 6 * (c - 7);
    else if (c >= 5) f = 77.25 + 6 * (c - 5); else f = 55.25 + 6 * (c - 2);
    printf "%.0f", f * 1e6 }')
# fH = 15734.2657 Hz; the LO sits 122 lines above the picture carrier, the
# sample rate is 432 samples per line, and one frame is 525 lines.
lo=$(awk -v p="$picture" 'BEGIN { printf "%.0f", p + 122 * (2 * 315e6 / 88 / 455) }')
rate=$(awk 'BEGIN { printf "%.0f", 432 * (2 * 315e6 / 88 / 455) }')

echo "channel $channel: picture carrier $picture Hz, LO $lo Hz, $rate S/s, -$ATTEN dB" >&2
# One hardware block = one frame, repeated on underrun: a late host is a
# freeze-frame, not a lost-sync roll.
"$PLUTOTV/host/ntsctx" "$@" --out - |
    "$PMP" tx --freq "$lo" --rate "$rate" --format ci8 --atten "$ATTEN" \
              --block-samples 226800 --repeat-on-underrun
