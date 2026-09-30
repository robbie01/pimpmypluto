#!/bin/sh
# tools/mkrescue-dtb.sh OUT.dtb   -- the stock device tree with our ramoops
# region (os/kernel/pluto.dts) fenced off, so the stock kernel booted as a
# rescue image leaves the last crash log in DDR for tools/lastlog.py.
set -eu
SRC=${DTB:-$(dirname "$0")/../vendor/stock/revb.dtb}
dtc -q -I dtb -O dts "$SRC" | awk '
    # right after the top-level memory node
    /^\tmemory \{/ { inmem = 1 }
    { print }
    inmem && /^\t\};/ {
        inmem = 0
        print "\n\treserved-memory {\n\t\t#address-cells = <0x01>;\n\t\t#size-cells = <0x01>;\n\t\tranges;\n"
        print "\t\tlastlog@1bf00000 {\n\t\t\treg = <0x1bf00000 0x100000>;\n\t\t\tno-map;\n\t\t};\n\t};"
    }' | dtc -q -I dts -O dtb -o "$1" -
