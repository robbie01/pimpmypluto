#!/usr/bin/env python3
"""Assemble the initramfs and FIT image.

    os/mkimage.py --kernel zImage --dtb board.dtb --bitstream system.bit.bin \
                  --out build/pluto.itb

The cpio is written directly (newc format) rather than with cpio(1) so that
files can be owned by root and device nodes created without being root on the
build host -- and so the build behaves the same on macOS and Linux.
"""
import argparse, base64, gzip, io, os, stat, struct, subprocess, sys, tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
OVERLAY = ROOT / "os" / "rootfs"
SECRETS = ROOT / "build" / "secrets"

# U-Boot picks config@N from the board's hardware-revision straps.  We ship one
# device tree, so every name the stock image defines maps to it.
FIT_CONFIGS = 11


class Cpio:
    def __init__(self):
        self.buf = io.BytesIO()
        self.ino = 1
        self.seen = set()

    def _entry(self, name, mode, data=b"", rdev=(0, 0)):
        name = name.strip("/")
        if name in self.seen:
            return
        self.seen.add(name)
        n = name.encode() + b"\0"
        hdr = "070701" + "".join("%08X" % v for v in (
            self.ino, mode, 0, 0, 1, 0, len(data), 0, 0, rdev[0], rdev[1], len(n), 0))
        self.ino += 1
        self.buf.write(hdr.encode() + n)
        self.buf.write(b"\0" * (-self.buf.tell() % 4))
        self.buf.write(data)
        self.buf.write(b"\0" * (-self.buf.tell() % 4))

    def dir(self, name, mode=0o755):
        parts = name.strip("/").split("/")
        for i in range(1, len(parts)):
            self._entry("/".join(parts[:i]), stat.S_IFDIR | 0o755)
        self._entry(name, stat.S_IFDIR | mode)

    def file(self, name, data, mode=0o644):
        if "/" in name.strip("/"):
            self.dir(os.path.dirname(name.strip("/")))
        self._entry(name, stat.S_IFREG | mode, data)

    def symlink(self, name, target):
        self._entry(name, stat.S_IFLNK | 0o777, target.encode())

    def chardev(self, name, major, minor, mode=0o600):
        self._entry(name, stat.S_IFCHR | mode, rdev=(major, minor))

    def tree(self, src):
        for path in sorted(Path(src).rglob("*")):
            rel = str(path.relative_to(src))
            if path.name == ".gitkeep":
                continue
            if path.is_symlink():
                self.symlink(rel, os.readlink(path))
            elif path.is_dir():
                self.dir(rel)
            else:
                mode = 0o755 if os.access(path, os.X_OK) else 0o644
                self.file(rel, path.read_bytes(), mode)

    def finish(self):
        self._entry("TRAILER!!!", 0)
        return self.buf.getvalue()


def ssh_string(b):
    return struct.pack(">I", len(b)) + b


def host_key():
    """A host key that survives reboots without touching the device's flash:
    generated once on the build host, baked into every image.  Returns it in
    dropbear's on-disk format."""
    SECRETS.mkdir(parents=True, exist_ok=True)
    key = SECRETS / "ssh_host_ed25519"
    if not key.exists():
        subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", "pimpmypluto",
                        "-f", str(key)], check=True)
    body = base64.b64decode("".join(l for l in key.read_text().splitlines()
                                    if not l.startswith("-----")))
    # openssh-key-v1, unencrypted: the 64-byte private blob (seed || public) is
    # the last-but-one string; find it by its unmistakable length prefix after
    # the public key that precedes it.
    marker = ssh_string(b"ssh-ed25519")
    at = body.rindex(marker) + len(marker)
    pub = body[at + 4:at + 36]
    at += 36
    (plen,) = struct.unpack(">I", body[at:at + 4])
    priv = body[at + 4:at + 4 + plen]
    assert plen == 64 and priv[32:] == pub, "unexpected OpenSSH key layout"
    return marker + ssh_string(priv)


def authorized_keys():
    keys = []
    for p in sorted(Path.home().glob(".ssh/id_*.pub")):
        keys.append(p.read_text().strip())
    extra = SECRETS / "authorized_keys"
    if extra.exists():
        keys.append(extra.read_text().strip())
    if not keys:
        sys.exit("no ~/.ssh/id_*.pub and no build/secrets/authorized_keys: image would be unreachable")
    return ("\n".join(keys) + "\n").encode()


def build_rootfs(bindir, version, no_serve=False, moddir=None, fake_rx=False):
    c = Cpio()
    for d in ("bin", "sbin", "usr/bin", "usr/sbin", "proc", "sys", "dev", "tmp", "run",
              "root", "etc/dropbear", "var/log", "mnt"):
        c.dir(d)
    c.dir("root/.ssh", 0o700)
    c.chardev("dev/console", 5, 1)
    c.chardev("dev/null", 1, 3, 0o666)
    c.tree(OVERLAY)
    for name, dest in (("busybox", "bin/busybox"), ("mdnsd", "usr/sbin/mdnsd"),
                       ("dropbearmulti", "usr/sbin/dropbearmulti")):
        c.file(dest, (Path(bindir) / name).read_bytes(), 0o755)
    for extra in sorted(Path(bindir).glob("extra/*")):
        c.file("usr/bin/" + extra.name, extra.read_bytes(), 0o755)
    for ko in sorted(Path(moddir or Path(bindir) / "modules").glob("*.ko")):
        c.file("lib/modules/" + ko.name, ko.read_bytes())
    c.symlink("bin/sh", "busybox")
    c.symlink("usr/sbin/dropbear", "dropbearmulti")
    c.symlink("usr/bin/scp", "../sbin/dropbearmulti")
    c.symlink("usr/bin/dropbearkey", "../sbin/dropbearmulti")
    c.file("etc/dropbear/dropbear_ed25519_host_key", host_key(), 0o600)
    c.file("root/.ssh/authorized_keys", authorized_keys(), 0o600)
    c.file("etc/pmp-version", version.encode() + b"\n")
    if no_serve:
        c.file("etc/pmp-no-serve", b"")
    if fake_rx:
        c.file("etc/pmp-fake-rx", b"")
    return gzip.compress(c.finish(), 9, mtime=0)


ITS = """/dts-v1/;
/ {
    description = "pimpmypluto %(version)s";
    #address-cells = <1>;
    images {
        fdt@1 {
            description = "devicetree";
            data = /incbin/("%(dtb)s");
            type = "flat_dt"; arch = "arm"; compression = "none";
        };
        fpga@1 {
            description = "FPGA";
            data = /incbin/("%(bitstream)s");
            type = "fpga"; arch = "arm"; compression = "none";
            load = <0xF000000>;
            hash@1 { algo = "md5"; };
        };
        linux_kernel@1 {
            description = "Linux";
            data = /incbin/("%(kernel)s");
            type = "kernel"; arch = "arm"; os = "linux"; compression = "none";
            load = <0x8000>; entry = <0x8000>;
            hash@1 { algo = "md5"; };
        };
        ramdisk@1 {
            description = "Ramdisk";
            data = /incbin/("%(ramdisk)s");
            type = "ramdisk"; arch = "arm"; os = "linux"; compression = "gzip";
            hash@1 { algo = "md5"; };
        };
    };
    configurations {
        default = "config@0";
%(configs)s
    };
};
"""

CONFIG = """        config@%d {
            description = "pimpmypluto";
            fdt = "fdt@1"; kernel = "linux_kernel@1"; ramdisk = "ramdisk@1"; fpga = "fpga@1";
        };"""


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--kernel", required=True)
    ap.add_argument("--dtb", required=True)
    ap.add_argument("--bitstream", required=True)
    ap.add_argument("--bindir", default=str(ROOT / "build" / "out"))
    ap.add_argument("--moddir", help="kernel modules (default: BINDIR/modules); they must match --kernel")
    ap.add_argument("--out", required=True)
    ap.add_argument("--no-serve", action="store_true", help="diagnostic image: do not start sdrd")
    ap.add_argument("--fake-rx", action="store_true", help="bug-hunting image: sdrd serves a synthetic RX stream, no DMA")
    a = ap.parse_args()

    version = subprocess.run(["git", "-C", str(ROOT), "describe", "--always", "--dirty"],
                             capture_output=True, text=True).stdout.strip() or "dev"
    with tempfile.TemporaryDirectory() as tmp:
        ramdisk = Path(tmp) / "rootfs.cpio.gz"
        ramdisk.write_bytes(build_rootfs(a.bindir, version, a.no_serve, a.moddir, a.fake_rx))
        its = Path(tmp) / "image.its"
        its.write_text(ITS % dict(
            version=version, dtb=os.path.abspath(a.dtb), bitstream=os.path.abspath(a.bitstream),
            kernel=os.path.abspath(a.kernel), ramdisk=ramdisk,
            configs="\n".join(CONFIG % i for i in range(FIT_CONFIGS))))
        r = subprocess.run(["mkimage", "-f", str(its), a.out], capture_output=True, text=True)
        if r.returncode:
            sys.exit(r.stdout + r.stderr)
        print("%s: %d bytes (ramdisk %d)" % (a.out, os.path.getsize(a.out), ramdisk.stat().st_size))


if __name__ == "__main__":
    main()
