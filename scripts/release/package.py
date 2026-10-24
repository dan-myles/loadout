#!/usr/bin/env python3
"""Build the loadout binary for one target and package it reproducibly.

Writes loadout-<target>.tar.gz (or .zip for Windows) plus a
sha256sum-compatible .sha256 file. Uses only the Python standard library,
cargo, rustc and git, so it runs unchanged on Linux, macOS and Windows.
"""

import argparse
import gzip
import hashlib
import io
import json
import os
import platform
import subprocess
import sys
import tarfile
import time
import zipfile
from pathlib import Path

PACKAGE = "loadout"
EXTRA_FILES = ("README.md", "LICENSE", "LICENSE-MIT", "LICENSE-APACHE")
ZIP_EPOCH_FLOOR = 315532800  # 1980-01-01, the earliest timestamp zip can store


class ReleaseError(Exception):
    pass


def run(cmd, cwd=None, env=None):
    print("+ " + " ".join(cmd), file=sys.stderr, flush=True)
    result = subprocess.run(cmd, cwd=cwd, env=env, stdout=subprocess.PIPE, text=True)
    if result.returncode != 0:
        raise ReleaseError(f"command failed with exit {result.returncode}: {' '.join(cmd)}")
    return result.stdout


def cargo_package(root):
    metadata = json.loads(
        run(["cargo", "metadata", "--no-deps", "--locked", "--format-version", "1"], cwd=root)
    )
    for package in metadata["packages"]:
        if package["name"] != PACKAGE:
            continue
        if not any(t["name"] == PACKAGE and "bin" in t["kind"] for t in package["targets"]):
            raise ReleaseError(f"package {PACKAGE} has no binary target named {PACKAGE}")
        return package["version"], Path(metadata["target_directory"])
    raise ReleaseError(f"no package named {PACKAGE} in {root / 'Cargo.toml'}")


def source_date_epoch(root):
    value = os.environ.get("SOURCE_DATE_EPOCH") or run(
        ["git", "log", "-1", "--format=%ct"], cwd=root
    ).strip()
    return int(value)


def is_windows(target):
    return "-windows-" in target


def build_env(root, target, epoch):
    cargo_home = Path(os.environ.get("CARGO_HOME") or Path.home() / ".cargo")
    rustflags = [
        f"--remap-path-prefix={root}=/build",
        f"--remap-path-prefix={cargo_home}=/cargo",
    ]
    if target.endswith("-windows-msvc"):
        # Static CRT avoids requiring the VC++ redistributable; /Brepro and
        # /PDBALTPATH keep the link timestamp and absolute PDB path out of the exe.
        rustflags += [
            "-C", "target-feature=+crt-static",
            "-C", "link-arg=/Brepro",
            "-C", "link-arg=/PDBALTPATH:%_PDB%",
        ]
    env = dict(os.environ)
    env.pop("RUSTFLAGS", None)
    env["CARGO_ENCODED_RUSTFLAGS"] = "\x1f".join(rustflags)
    env["SOURCE_DATE_EPOCH"] = str(epoch)
    env["CARGO_INCREMENTAL"] = "0"
    return env


def host_can_run(target):
    machine = platform.machine().lower()
    arch = {"amd64": "x86_64", "x64": "x86_64", "arm64": "aarch64"}.get(machine, machine)
    system = {"darwin": "-apple-darwin", "linux": "-linux-", "windows": "-windows-"}.get(
        platform.system().lower()
    )
    return target.startswith(arch + "-") and system is not None and system in target


def smoke_test(binary, target, version, required=False):
    if not host_can_run(target):
        if required:
            raise ReleaseError(f"--require-smoke: this host cannot execute {target}")
        print(f"smoke test skipped: this host cannot execute {target}", file=sys.stderr)
        return
    output = run([str(binary), "--version"])
    if output.split()[:2] != [PACKAGE, version]:
        raise ReleaseError(
            f"{binary.name} --version printed {output.strip()!r}, expected '{PACKAGE} {version}'"
        )
    print(f"smoke test passed: {output.strip()}", file=sys.stderr)


def members(root, binary, target):
    prefix = f"{PACKAGE}-{target}"
    entries = [(f"{prefix}/{binary.name}", binary.read_bytes(), 0o755)]
    for name in EXTRA_FILES:
        path = root / name
        if path.is_symlink():
            raise ReleaseError(f"{name} is a symlink; release archives only ship regular files")
        if path.is_file():
            entries.append((f"{prefix}/{name}", path.read_bytes(), 0o644))
    return prefix, sorted(entries)


def write_tar_gz(path, prefix, entries, epoch):
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.USTAR_FORMAT) as tar:
        directory = tarfile.TarInfo(prefix)
        directory.type = tarfile.DIRTYPE
        directory.mode = 0o755
        directory.mtime = epoch
        tar.addfile(directory)
        for name, data, mode in entries:
            info = tarfile.TarInfo(name)
            info.size = len(data)
            info.mode = mode
            info.mtime = epoch
            tar.addfile(info, io.BytesIO(data))
    with open(path, "wb") as out, gzip.GzipFile(
        filename="", mode="wb", fileobj=out, compresslevel=9, mtime=epoch
    ) as gz:
        gz.write(raw.getvalue())


def write_zip(path, prefix, entries, epoch):
    stamp = time.gmtime(max(epoch, ZIP_EPOCH_FLOOR))[:6]
    with zipfile.ZipFile(path, "w") as archive:
        for name, data, mode in [(prefix + "/", b"", 0o755)] + entries:
            info = zipfile.ZipInfo(name, date_time=stamp)
            info.create_system = 3
            info.external_attr = (mode | (0o040000 if name.endswith("/") else 0o100000)) << 16
            info.compress_type = zipfile.ZIP_STORED if name.endswith("/") else zipfile.ZIP_DEFLATED
            archive.writestr(info, data, compresslevel=9)


def write_archive(out_dir, prefix, entries, target, epoch):
    out_dir.mkdir(parents=True, exist_ok=True)
    if is_windows(target):
        path = out_dir / f"{prefix}.zip"
        write_zip(path, prefix, entries, epoch)
    else:
        path = out_dir / f"{prefix}.tar.gz"
        write_tar_gz(path, prefix, entries, epoch)
    digest = hashlib.sha256(path.read_bytes()).hexdigest()
    checksum = path.with_name(path.name + ".sha256")
    checksum.write_bytes(f"{digest}  {path.name}\n".encode("ascii"))
    return path, checksum


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--target", required=True, help="Rust target triple to build")
    parser.add_argument("--out-dir", type=Path, help="default: <cargo target dir>/dist")
    parser.add_argument(
        "--expect-version", help="fail unless Cargo.toml's version equals this (tag without v)"
    )
    parser.add_argument(
        "--require-smoke",
        action="store_true",
        help="fail instead of skipping the --version smoke test when the host can't run the target",
    )
    args = parser.parse_args(argv)

    root = Path(run(["git", "rev-parse", "--show-toplevel"]).strip())
    version, target_dir = cargo_package(root)
    if args.expect_version and args.expect_version != version:
        raise ReleaseError(
            f"tag version {args.expect_version} does not match Cargo.toml version {version}"
        )
    epoch = source_date_epoch(root)

    print(run(["rustc", "-vV"], cwd=root), file=sys.stderr)
    run(
        ["cargo", "build", "--release", "--locked", "--target", args.target, "--bin", PACKAGE],
        cwd=root,
        env=build_env(root, args.target, epoch),
    )
    exe = PACKAGE + (".exe" if is_windows(args.target) else "")
    binary = target_dir / args.target / "release" / exe
    smoke_test(binary, args.target, version, args.require_smoke)

    prefix, entries = members(root, binary, args.target)
    out_dir = (args.out_dir or target_dir / "dist").resolve()
    archive, checksum = write_archive(out_dir, prefix, entries, args.target, epoch)
    print(checksum.read_text(encoding="ascii"), end="")
    print(archive)


if __name__ == "__main__":
    try:
        main()
    except ReleaseError as error:
        print(f"error: {error}", file=sys.stderr)
        sys.exit(1)
