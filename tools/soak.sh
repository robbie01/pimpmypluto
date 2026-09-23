#!/bin/sh
# Session soak: N rounds of full-duplex loopback across every format pairing.
# Every session opens and closes both streams, which is where the bugs live.
#   tools/soak.sh [rounds]        (needs target/release/pmp)
cd "$(dirname "$0")/.." || exit 1
rounds=${1:-6}; n=0; bad=0
for round in $(seq 1 "$rounds"); do
    for spec in "ci8 ci8" "ci12 ci16" "ci16 ci12" "ci12 ci12" "ci8 ci12" "ci12 ci8" "ci16 ci16"; do
        set -- $spec; n=$((n + 1))
        r=$(./target/release/pmp loopback --freq 915M --rate 4M --format "$1" --rx-format "$2" 2>&1 | grep -E "tone carries|pmp:" | head -1)
        if echo "$r" | grep -q -E "pmp:|carries [0-8]?[0-9]\."; then
            bad=$((bad + 1)); echo "session $n (tx $1 rx $2) BAD: $r"
            echo "$r" | grep -q "no pimpmypluto" && { echo "device gone after $n sessions"; exit 2; }
        fi
    done
    echo "round $round: $n sessions, $bad bad"
done
[ "$bad" = 0 ]
