# Third-party reference: the "PlutoSDR Nano" board definition

This board is the AliExpress "PlutoSDR Nano" (listing title: "New Zynq7010
AD9363 SDR Software Defined Radio Development Board Support FLASH and JTAG Boot
Modes for PlutoSDR Nano"). F5OEO's tezuka_fw supports it as board `nano`
(https://github.com/F5OEO/tezuka_fw, issue #227), and the files here are
unmodified copies, fetched 2026-09-21, kept for reference only:

| file | from |
|---|---|
| `system_constr.xdc`, `system_top.v`, `ps7.tcl`, `ports.tcl` | `F5OEO/maia-sdr`, branch `refactor`, `maia-hdl/projects/boards/nano/` |
| `nano.png`, `nano_BOOT_MODE_COMPLETE_GUIDE.md` | `F5OEO/tezuka_fw`, `doc/schematics/` |

They are someone else's work under their licence; nothing in pimpmypluto is
derived from them yet. The seller's own pin table (a Chinese PDF on Baidu
Drive, linked from the issue) is the primary source behind the XDC.

What agrees with measurements on this unit: XC7Z010-CLG400, one 16-bit
MT41K256M16, USB PHY reset on MIO 46, UART1 on MIO 12/13, single-SS QSPI, CMOS
(not LVDS) AD9363 interface at 1.8 V. What has **not** been checked here: the
PL pin assignments themselves. Tezuka's bitstream for this board works for its
users, which is decent evidence; the way to check on this unit is to build a
bitstream from them and see the radio come up.

Note for anyone running tezuka_fw on this board: its FSBL (`ps7_init.c` in
`board/tezuka/nano/bitstream/tezuka.xsa`) programs the DDR address pins with
the same Xilinx default drive/slew as the vendor's (0xF8000B5C = 0x0018C61C),
which is what made this unit corrupt memory under load. See "DDR address bus"
in docs/ARCHITECTURE.md; their `doc/crashnano.md` chases what looks like the
same family of fault on a sibling board.
