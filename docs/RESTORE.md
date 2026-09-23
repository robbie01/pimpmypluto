# Getting back to stock

> **Once `tools/flash.sh` has been run** the board boots pimpmypluto from flash
> and U-Boot's environment carries the DDR fixes (`preboot`: address, data and
> strobe pin slew; current env image `build/flash/uboot-env-pmp2.bin`, built
> by `tools/mkenv.py`). To undo both:
> hold the button while plugging in (flash DFU), then
> `tools/restore-stock.sh --go`. FSBL/U-Boot are never written by either tool.
> Holding the button also *skips* the `preboot` DDR write, so that change can
> never lock out this path.

Before that, nothing had been written to flash: images were booted from RAM,
and unplugging returned the board to stock. RAM booting (`tools/ramboot.sh`) is
still how development is done; it never touches flash, and a replug returns to
whatever *is* in flash.

## The backup

`~/pluto-backup-20260919/` holds a byte-exact dump of the whole 32 MB flash,
taken from the running stock firmware and MD5-verified against the device:

| file | flash range | contents |
|---|---|---|
| `mtd0.bin` | 0x000000–0x0FFFFF | FSBL + U-Boot (**never written by us**) |
| `mtd1.bin` | 0x100000–0x11FFFF | U-Boot environment |
| `mtd2.bin` | 0x120000–0x1FFFFF | nvmfs (jffs2) |
| `mtd3.bin` | 0x200000–0x1FFFFFF | firmware FIT: bitstream + kernel + rootfs |

This vendor build exists nowhere else. Copy that directory somewhere safe.

## Restoring by hand

`tools/restore-stock.sh` does exactly this. U-Boot's DFU mode can rewrite
everything except itself. Enter it with
`reboot-to sf` (ours), `device_reboot sf` (stock), or by holding the button
while plugging in. Then:

```sh
dfu-util -d 0456:b674 -a firmware.dfu  -D ~/pluto-backup-20260919/mtd3.bin
dfu-util -d 0456:b674 -a uboot-env.dfu -D ~/pluto-backup-20260919/mtd1.bin   # only if the env was changed
dfu-util -d 0456:b674 -a firmware.dfu  -e
```

`boot.dfu` (mtd0) is only ever needed if U-Boot itself is damaged — which
requires deliberately writing it. If that happens DFU is gone too and recovery
needs JTAG, which is why the kernel marks that partition read-only (along with
the environment and the firmware: Linux has no business writing either).

## A RAM-booted image that cannot reach the host

Dev images carry a failsafe (`/etc/pmp-dev`): no USB host within 25 s and the
board returns to RAM-DFU by itself. If the kernel dies before init, replug.
Either way, `tools/ramboot.sh build/rescue.itb && tools/lastlog.py` shows the
dead kernel's console.
