#!/usr/bin/env python3
"""Build loopback release trees for the loadout acceptance campaign (vm-setup, standing 128).

Each variant is a folder holding one release asset and its `.sha256` sidecar, laid out like
`scripts/release/package.py` writes them, so `install.sh --base-url http://127.0.0.1:PORT/<variant>`
and `install.ps1 -BaseUrl ...` can be pointed at it inside a guest. Nothing here downloads, builds,
or runs the CLI; inputs are files the campaign was granted (CI release archives or host builds).

  release_fixture.py linux   --archive loadout-x86_64-unknown-linux-musl.tar.gz --out DIR
  release_fixture.py linux   --binary target/.../loadout --version 0.1.0 --out DIR
  release_fixture.py windows --archive loadout-x86_64-pc-windows-msvc.zip --out DIR
  release_fixture.py windows --exe loadout.exe --launcher loadout-background.exe --version 0.1.0 \
                             --fake-newer-dir DIR_WITH_9.9.9_EXES --out DIR

The real binaries are never modified; only archives and sidecars around them differ per variant.
"""

import argparse
import gzip
import hashlib
import io
import json
import sys
import tarfile
import time
import zipfile
from pathlib import Path

LINUX_TARGET = "x86_64-unknown-linux-musl"
WINDOWS_TARGET = "x86_64-pc-windows-msvc"
EPOCH = 1700000000
ZIP_STAMP = time.gmtime(EPOCH)[:6]
FAKE_NEWER = "9.9.9"


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def fake_unix_binary(name, version):
    """A shell stand-in that answers `--version` like a release binary; never the CLI under test."""
    text = (
        "#!/bin/sh\n"
        "# loadout acceptance fixture: not the CLI under test\n"
        'if [ "${1:-}" = --version ]; then echo "%s %s"; exit 0; fi\n'
        "exit 2\n"
    ) % (name, version)
    return text.encode("ascii")


def tar_gz(prefix, entries, extra=()):
    """entries: (name, bytes, mode) under prefix/; extra: raw TarInfo-producing tuples."""
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.USTAR_FORMAT) as tar:
        directory = tarfile.TarInfo(prefix)
        directory.type = tarfile.DIRTYPE
        directory.mode = 0o755
        directory.mtime = EPOCH
        tar.addfile(directory)
        for name, data, mode in entries:
            info = tarfile.TarInfo(name)
            info.size = len(data)
            info.mode = mode
            info.mtime = EPOCH
            tar.addfile(info, io.BytesIO(data))
        for kind, name, target in extra:
            info = tarfile.TarInfo(name)
            info.mtime = EPOCH
            info.mode = 0o755
            info.type = tarfile.SYMTYPE if kind == "symlink" else tarfile.LNKTYPE
            info.linkname = target
            tar.addfile(info)
    out = io.BytesIO()
    with gzip.GzipFile(filename="", mode="wb", fileobj=out, compresslevel=9, mtime=EPOCH) as gz:
        gz.write(raw.getvalue())
    return out.getvalue()


def zip_bytes(prefix, entries):
    out = io.BytesIO()
    with zipfile.ZipFile(out, "w") as archive:
        for name, data, mode in [(prefix + "/", b"", 0o755)] + entries:
            info = zipfile.ZipInfo(name, date_time=ZIP_STAMP)
            info.create_system = 3
            info.external_attr = (mode | (0o040000 if name.endswith("/") else 0o100000)) << 16
            info.compress_type = zipfile.ZIP_STORED if name.endswith("/") else zipfile.ZIP_DEFLATED
            archive.writestr(info, data)
    return out.getvalue()


def read_linux_archive(path, target=LINUX_TARGET):
    with tarfile.open(path, "r:gz") as tar:
        members = {m.name.rstrip("/"): m for m in tar.getmembers()}
        prefix = "loadout-" + target
        binary = members.get(prefix + "/loadout")
        if binary is None or not binary.isfile():
            sys.exit("%s has no regular %s/loadout" % (path, prefix))
        others = []
        for name, member in sorted(members.items()):
            if member.isfile() and name != prefix + "/loadout":
                others.append((name, tar.extractfile(member).read(), 0o644))
        return tar.extractfile(binary).read(), others


def read_windows_archive(path):
    prefix = "loadout-" + WINDOWS_TARGET
    with zipfile.ZipFile(path) as archive:
        names = archive.namelist()
        exe = archive.read(prefix + "/loadout.exe")
        launcher = archive.read(prefix + "/loadout-background.exe")
        others = [
            (name, archive.read(name), 0o644)
            for name in names
            if not name.endswith("/") and name not in (prefix + "/loadout.exe", prefix + "/loadout-background.exe")
        ]
    return exe, launcher, others


class Tree:
    def __init__(self, out):
        self.out = out
        self.manifest = {}

    def write(self, variant, asset, data, sidecar=None):
        folder = self.out / variant
        folder.mkdir(parents=True, exist_ok=False)
        (folder / asset).write_bytes(data)
        if sidecar is None:
            sidecar = ("%s  %s\n" % (sha256(data), asset)).encode("ascii")
        (folder / (asset + ".sha256")).write_bytes(sidecar)
        self.manifest[variant] = {
            "asset": asset,
            "asset_sha256": sha256(data),
            "sidecar_sha256": sha256(sidecar),
            "sidecar": sidecar.decode("ascii", "replace"),
        }

    def finish(self, meta):
        (self.out / "manifest.json").write_text(
            json.dumps({"meta": meta, "variants": self.manifest}, indent=2, sort_keys=True) + "\n"
        )


def linux(args):
    """Unix tar.gz trees: musl Linux by default, or an apple-darwin --target for the macOS recipe."""
    prefix = "loadout-" + args.target
    asset = prefix + ".tar.gz"
    if args.archive:
        original = Path(args.archive).read_bytes()
        binary, others = read_linux_archive(args.archive, args.target)
    else:
        original = None
        binary = Path(args.binary).read_bytes()
        others = []
    good = original or tar_gz(prefix, [(prefix + "/loadout", binary, 0o755)] + others)
    tree = Tree(Path(args.out))
    tree.write("good", asset, good)
    tree.write("bad-sha", asset, good, ("%s  %s\n" % ("0" * 64, asset)).encode("ascii"))
    tree.write("bad-sidecar", asset, good, ("%s %s\n" % (sha256(good), asset)).encode("ascii"))
    tree.write("sidecar-two-lines", asset, good, ("%s  %s\n%s  %s\n" % (sha256(good), asset, sha256(good), asset)).encode("ascii"))
    tree.write("outside-entry", asset, tar_gz(prefix, [(prefix + "/loadout", binary, 0o755), ("evil.txt", b"x\n", 0o644)]))
    tree.write("nested-entry", asset, tar_gz(prefix, [(prefix + "/loadout", binary, 0o755), (prefix + "/sub/x", b"x\n", 0o644)]))
    tree.write("duplicate-binary", asset, tar_gz(prefix, [(prefix + "/loadout", binary, 0o755), (prefix + "/loadout", binary, 0o755)]))
    tree.write("symlink-binary", asset, tar_gz(prefix, [], [("symlink", prefix + "/loadout", "/bin/sh")]))
    tree.write("missing-binary", asset, tar_gz(prefix, [(prefix + "/README.md", b"x\n", 0o644)]))
    tree.write("not-runnable", asset, tar_gz(prefix, [(prefix + "/loadout", b"\x7fELF-not-really\n", 0o755)]))
    tree.write("newer-fake", asset, tar_gz(prefix, [(prefix + "/loadout", fake_unix_binary("loadout", FAKE_NEWER), 0o755)]))
    tree.write("wrong-name-fake", asset, tar_gz(prefix, [(prefix + "/loadout", fake_unix_binary("other", FAKE_NEWER), 0o755)]))
    tree.finish({"os": "macos" if "apple-darwin" in args.target else "linux", "target": args.target,
                 "binary_sha256": sha256(binary),
                 "from_ci_archive": bool(args.archive), "version": args.version})


def windows(args):
    prefix = "loadout-" + WINDOWS_TARGET
    asset = prefix + ".zip"
    if args.archive:
        original = Path(args.archive).read_bytes()
        exe, launcher, others = read_windows_archive(args.archive)
    else:
        original = None
        exe = Path(args.exe).read_bytes()
        launcher = Path(args.launcher).read_bytes()
        others = []
    pair = [(prefix + "/loadout.exe", exe, 0o755), (prefix + "/loadout-background.exe", launcher, 0o755)]
    good = original or zip_bytes(prefix, pair + others)
    tree = Tree(Path(args.out))
    tree.write("good", asset, good)
    tree.write("bad-sha", asset, good, ("%s  %s\n" % ("0" * 64, asset)).encode("ascii"))
    tree.write("bad-sidecar", asset, good, ("%s %s\n" % (sha256(good), asset)).encode("ascii"))
    tree.write("missing-launcher", asset, zip_bytes(prefix, [pair[0]]))
    tree.write("missing-main", asset, zip_bytes(prefix, [pair[1]]))
    tree.write("outside-entry", asset, zip_bytes(prefix, pair + [("evil.txt", b"x\n", 0o644)]))
    # Schema-1 era layout (one binary) for the c9f7fd0 installer, to drive the schema-1 -> 2 upgrade.
    tree.write("schema1-single", asset, zip_bytes(prefix, [pair[0]]))
    if args.fake_newer_dir:
        fake = Path(args.fake_newer_dir)
        newer_main = (fake / "loadout.exe").read_bytes()
        newer_launcher = (fake / "loadout-background.exe").read_bytes()
        tree.write("newer-fake", asset, zip_bytes(prefix, [
            (prefix + "/loadout.exe", newer_main, 0o755),
            (prefix + "/loadout-background.exe", newer_launcher, 0o755)]))
        # Launcher from another version than loadout.exe: the installer must refuse the pair.
        tree.write("mismatched-pair", asset, zip_bytes(prefix, [
            (prefix + "/loadout.exe", exe, 0o755),
            (prefix + "/loadout-background.exe", newer_launcher, 0o755)]))
    tree.finish({"os": "windows", "target": WINDOWS_TARGET, "exe_sha256": sha256(exe),
                 "launcher_sha256": sha256(launcher), "from_ci_archive": bool(args.archive),
                 "version": args.version})


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="os", required=True)
    lin = sub.add_parser("linux")
    src = lin.add_mutually_exclusive_group(required=True)
    src.add_argument("--archive")
    src.add_argument("--binary")
    lin.add_argument("--version", required=True)
    lin.add_argument("--target", default=LINUX_TARGET,
                     choices=(LINUX_TARGET, "aarch64-unknown-linux-musl", "aarch64-apple-darwin", "x86_64-apple-darwin"))
    lin.add_argument("--out", required=True)
    win = sub.add_parser("windows")
    wsrc = win.add_mutually_exclusive_group(required=True)
    wsrc.add_argument("--archive")
    wsrc.add_argument("--exe")
    win.add_argument("--launcher")
    win.add_argument("--fake-newer-dir")
    win.add_argument("--version", required=True)
    win.add_argument("--out", required=True)
    args = parser.parse_args(argv)
    out = Path(args.out)
    if out.exists() and any(out.iterdir()):
        sys.exit("%s must be absent or empty" % out)
    out.mkdir(parents=True, exist_ok=True)
    if args.os == "linux":
        linux(args)
    else:
        if args.exe and not args.launcher:
            sys.exit("--exe needs --launcher")
        windows(args)
    print(json.dumps(json.loads((out / "manifest.json").read_text())["variants"], sort_keys=True))


if __name__ == "__main__":
    main()
