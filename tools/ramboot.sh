#!/bin/sh
# Boot a FIT image on the Pluto from RAM.  Writes nothing to flash: a power
# cycle always returns to whatever is in QSPI.
#
#   tools/ramboot.sh build/pluto.itb
#
# Works from three starting states:
#   - already in U-Boot RAM-DFU mode (0456:b674)
#   - running our firmware   (ssh key auth, `reboot-to ram`)
#   - running stock firmware (password "analog", `device_reboot ram`)
set -eu
IMG=${1:?usage: $0 image.itb}
# Our firmware is IPv6 link-local only (mDNS name); stock answers on IPv4.
ADDRS=${PLUTO_ADDR:-"pluto.local 192.168.2.1"}
DFU_ID=0456:b674
SSHO="-o ConnectTimeout=5 -o ServerAliveInterval=2 -o ServerAliveCountMax=2 -o UserKnownHostsFile=/dev/null -o StrictHostKeyChecking=no -o LogLevel=ERROR"

in_dfu() { dfu-util -l 2>/dev/null | grep -q "$DFU_ID"; }

# U-Boot's flash-DFU mode has the same USB ID and also an alt setting called
# firmware.dfu -- but there a download is written to QSPI.  Only RAM-DFU mode
# exposes "dummy.dfu"; refuse to upload anywhere else.
in_ram_dfu() { dfu-util -l 2>/dev/null | grep "$DFU_ID" | grep -q 'name="dummy.dfu"'; }

if ! in_dfu; then
    echo ">> asking device to reboot into RAM-DFU"
    # The stock image remounts its mass-storage volume on the host; let go of
    # it first so macOS does not complain about an unsafe eject.
    diskutil unmount /Volumes/PlutoSDR >/dev/null 2>&1 || true
    asked=
    for ADDR in $ADDRS; do
        # shellcheck disable=SC2086
        if ssh $SSHO -o BatchMode=yes "root@$ADDR" 'reboot-to ram' 2>/dev/null; then asked=1; break; fi
    done
    if [ -z "$asked" ]; then
        ASKPASS=$(mktemp "${TMPDIR:-/tmp}/askpass.XXXXXX")
        printf '#!/bin/sh\necho analog\n' > "$ASKPASS"; chmod 700 "$ASKPASS"
        for ADDR in $ADDRS; do
            # shellcheck disable=SC2086
            SSH_ASKPASS="$ASKPASS" SSH_ASKPASS_REQUIRE=force \
                ssh $SSHO -o PubkeyAuthentication=no "root@$ADDR" \
                'devmem 0xF8000008 32 0xDF0D; devmem 0xF8000258 32 0x00400007; sync; reboot -f' 2>/dev/null && break  # see os/rootfs/usr/sbin/reboot-to.  -f: stock's shutdown scripts can hang forever, leaving a board only a replug fixes
        done
        rm -f "$ASKPASS"
    fi
    n=0
    until in_dfu; do
        n=$((n + 1))
        [ $n -lt 40 ] || { echo "device never entered DFU mode" >&2; exit 1; }
        sleep 1
    done
fi

in_ram_dfu || {
    echo "device is in *flash* DFU mode, not RAM-DFU: refusing to upload (it would write QSPI)." >&2
    echo "replug the device and try again." >&2
    exit 1
}
echo ">> uploading $IMG"
dfu-util -d "$DFU_ID" -a firmware.dfu -D "$IMG" 2>&1 | grep -E "^(Download done|Error|dfu-util:)" || true
dfu-util -d "$DFU_ID" -a firmware.dfu -e >/dev/null 2>&1 || true
echo ">> booting"
