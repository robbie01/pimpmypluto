#!/bin/sh
# tools/dtb-l2.sh OUT.dtb "prop = <v>; prop; ..."   -- variant of the built DTB
# with properties set (replacing any existing ones) on the PL310 node.
set -eu
SRC=${DTB:-build/cs/kbuild/arch/arm/boot/dts/xilinx/zynq-pmp-pluto.dtb}
dtc -q -I dtb -O dts "$SRC" | awk -v props="$2" '
    BEGIN { n = split(props, p, ";"); for (i = 1; i <= n; i++) { k = p[i]; sub(/^[ \t]+/, "", k); sub(/[ \t]*=.*/, "", k); sub(/[ \t]+$/, "", k); if (k != "") name[k] = 1 } }
    /cache-controller@f8f02000 \{/ { print; innode = 1; for (i = 1; i <= n; i++) if (p[i] ~ /[a-z]/) print "\t\t\t" p[i] ";"; next }
    innode && /\};/ { innode = 0 }
    innode { k = $1; sub(/;$/, "", k); if (k in name) next }
    { print }' | dtc -q -I dts -O dtb -o "$1" -
