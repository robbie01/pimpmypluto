#!/bin/sh
# Put the vendor's firmware and U-Boot environment back, byte for byte, from
# the flash backup.  Board must be in flash DFU (hold the button while plugging in).
#   tools/restore-stock.sh [--go] [BACKUP_DIR]
set -eu
GO=; [ "${1:-}" = "--go" ] && { GO=--go; shift; }
B=${1:-$HOME/pluto-backup-20260919}
# mtd3.bin is the whole 30 MiB partition; it begins with the vendor FIT.
exec "$(dirname "$0")/flash.sh" $GO "$B/mtd3.bin" "$B/mtd1.bin"
